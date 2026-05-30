#!/usr/bin/env bash
# scripts/prerelease.sh — run every pre-release gate locally.
#
# This is the local mirror of `.github/workflows/ci.yml`'s `check` job
# (fmt, clippy, build, test) PLUS the invariants CI does not enforce but
# the repo already configures: the parent-binary link-set assertion
# (CLAUDE.md "Multi-toolchain dispatch"), taplo/mdwright format checks
# (taplo.toml / .mdwright.toml / npx prettier), cargo-deny (deny.toml), and cargo-shear.
# Passing locally is the fast feedback loop before a release tag.
#
# Unlike its sibling repos, CI here does NOT set RUSTFLAGS=-D warnings
# globally — only clippy is `-D warnings`. We stay faithful to that.
#
# All gates are attempted even if an earlier one fails; the run ends with
# a pass/fail/skip summary and a non-zero exit if anything failed. Optional
# tools (taplo, mdwright, cargo-deny, cargo-shear) are SKIPPED, not failed,
# when absent — so the script runs on any dev box.
#
# Usage:
#   scripts/prerelease.sh            # all gates
#   scripts/prerelease.sh --quick    # skip the slow/network gates (cargo-deny)
#   scripts/prerelease.sh --help

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

QUICK=0

# -- logging ----------------------------------------------------------------

if [[ -t 1 ]]; then
	BOLD=$'\033[1m'
	GREEN=$'\033[32m'
	RED=$'\033[31m'
	YELLOW=$'\033[33m'
	RESET=$'\033[0m'
else
	BOLD="" GREEN="" RED="" YELLOW="" RESET=""
fi

log_step() { printf '\n%s==>%s %s%s%s\n' "$BOLD" "$RESET" "$BOLD" "$*" "$RESET"; }
log_ok() { printf '%s✓%s %s\n' "$GREEN" "$RESET" "$*"; }
log_warn() { printf '%s!%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
log_err() { printf '%s✗%s %s\n' "$RED" "$RESET" "$*" >&2; }

usage() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

# -- arg parsing ------------------------------------------------------------

while [[ $# -gt 0 ]]; do
	case "$1" in
	--quick)
		QUICK=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		log_err "unknown argument: $1"
		usage >&2
		exit 2
		;;
	esac
done

# -- preflight --------------------------------------------------------------

require_cmd() {
	if ! command -v "$1" >/dev/null 2>&1; then
		log_err "required command not found: $1${2:+ ($2)}"
		exit 2
	fi
}

require_cmd cargo "install via https://rustup.rs"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

# -- gate runner ------------------------------------------------------------

declare -a PASSED=() FAILED=() SKIPPED=()

run_gate() {
	local name="$1"
	shift
	log_step "$name"
	local start=$SECONDS
	if "$@"; then
		log_ok "$name ($((SECONDS - start))s)"
		PASSED+=("$name")
	else
		local rc=$?
		log_err "$name FAILED in $((SECONDS - start))s (exit $rc)"
		FAILED+=("$name")
	fi
}

# Skip a gate whose optional tool is missing (recorded, not failed).
skip_gate() {
	log_warn "skipping: $1"
	SKIPPED+=("$1")
}

# -- Rust gates (mirror ci.yml) ---------------------------------------------

run_gate "cargo fmt --all -- --check" \
	cargo fmt --all -- --check

# clippy --workspace is link-safe (no linking happens) per CLAUDE.md.
run_gate "cargo clippy --workspace --all-targets -- -D warnings" \
	cargo clippy --workspace --all-targets -- -D warnings

# Build and test PER-MEMBER, never workspace-wide. A `--workspace` /
# `--all-targets` build unifies the `lean-rs-sys` feature set across the
# parent and worker crates, silently re-linking `libleanshared` into the
# parent (CLAUDE.md "Always build per-member"). That both violates the
# parent ⊥ libleanshared invariant and makes the parent's own test binary
# unrunnable — it references `@rpath/libleanshared.dylib` with no rpath to
# find it. Scoping to `-p` keeps the parent on `lean-rs-sys` metadata-only,
# so its tests run on any machine without the dylib on the loader path.
run_gate "cargo build -p lean-host-mcp --all-targets" \
	cargo build -p lean-host-mcp --all-targets

run_gate "cargo build -p lean-host-mcp-worker --all-targets" \
	cargo build -p lean-host-mcp-worker --all-targets

# The worker crate has no tests of its own (a 2-line entry point); the worker
# build gate above is its coverage. Test the parent per-member.
run_gate "cargo test -p lean-host-mcp --all-targets" \
	cargo test -p lean-host-mcp --all-targets

# -- Key invariant: the parent must NOT link libleanshared ------------------
#
# Built as a SEPARATE per-member release invocation so the worker crate's
# feature set never unifies into the parent (a plain `--workspace` build
# would silently re-link the dylib). Then assert with the platform linker.
gate_link_invariant() {
	cargo build --release -p lean-host-mcp
	local bin="target/release/lean-host-mcp"
	[[ -x "$bin" ]] || {
		log_err "expected release binary not found: $bin"
		return 1
	}
	case "$(uname -s)" in
	Darwin)
		if otool -L "$bin" | grep -q libleanshared; then
			log_err "$bin links libleanshared — the parent must stay free of it (CLAUDE.md)."
			return 1
		fi
		;;
	Linux)
		if ldd "$bin" 2>/dev/null | grep -q libleanshared; then
			log_err "$bin links libleanshared — the parent must stay free of it (CLAUDE.md)."
			return 1
		fi
		;;
	*)
		log_warn "unknown OS '$(uname -s)'; skipping link-set assertion"
		;;
	esac
	return 0
}
run_gate "parent ⊥ libleanshared (release link-set)" gate_link_invariant

# -- Format checks beyond rustfmt (configured, not in CI) -------------------

if command -v taplo >/dev/null 2>&1; then
	run_gate "taplo fmt --check" taplo fmt --check
else
	skip_gate "taplo fmt --check (taplo not installed)"
fi

if command -v mdwright >/dev/null 2>&1; then
	run_gate "mdwright check" mdwright check
else
	skip_gate "mdwright check (mdwright not installed)"
fi

# prettier owns .json formatting (see .claude/hooks/format.sh). Check the
# repo's own JSON; fixtures/ holds Lake-generated manifests we don't format.
gate_prettier() {
	local files=()
	while IFS= read -r f; do files+=("$f"); done < <(git ls-files '*.json' ':(exclude)fixtures/')
	((${#files[@]})) || return 0
	npx --yes prettier --check "${files[@]}"
}
if command -v npx >/dev/null 2>&1; then
	run_gate "prettier --check (json)" gate_prettier
else
	skip_gate "prettier --check (npx not installed)"
fi

# -- Dependency hygiene (configured, not in CI) -----------------------------

if [[ "$QUICK" == 1 ]]; then
	skip_gate "cargo deny check (--quick)"
elif command -v cargo-deny >/dev/null 2>&1; then
	run_gate "cargo deny check" cargo deny check
else
	skip_gate "cargo deny check (cargo-deny not installed)"
fi

if command -v cargo-shear >/dev/null 2>&1; then
	run_gate "cargo shear" cargo shear
else
	skip_gate "cargo shear (cargo-shear not installed)"
fi

# -- summary ----------------------------------------------------------------

printf '\n%s====== Pre-release summary ======%s\n' "$BOLD" "$RESET"
printf '\npassed (%d):\n' "${#PASSED[@]}"
for name in "${PASSED[@]}"; do printf '  %s✓%s %s\n' "$GREEN" "$RESET" "$name"; done

if ((${#SKIPPED[@]} > 0)); then
	printf '\nskipped (%d):\n' "${#SKIPPED[@]}"
	for name in "${SKIPPED[@]}"; do printf '  %s-%s %s\n' "$YELLOW" "$RESET" "$name"; done
fi

if ((${#FAILED[@]} > 0)); then
	printf '\n%sfailed (%d):%s\n' "$RED" "${#FAILED[@]}" "$RESET"
	for name in "${FAILED[@]}"; do printf '  %s✗%s %s\n' "$RED" "$RESET" "$name"; done
	exit 1
fi

printf '\n%sAll gates passed.%s\n' "$GREEN" "$RESET"
