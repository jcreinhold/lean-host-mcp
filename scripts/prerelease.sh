#!/usr/bin/env bash
# scripts/prerelease.sh — run the local pre-release checks.
#
# Runs the Rust checks, parent link check, Lake-native check and cargo-deny
# from CI.
# Adds local taplo/prettier formatting, mdwright linting and cargo-shear.
# See AGENTS.md "Build and validation" for the per-member build invariant.
#
# Like CI, only clippy and rustdoc use -D warnings; RUSTFLAGS is not set globally.
#
# All gates are attempted even if an earlier one fails; the run ends with
# a pass/fail/skip summary and a non-zero exit if anything failed. Optional
# tools (rg, taplo, mdwright, prettier via npx, cargo-deny, cargo-shear) are
# skipped when absent — so the script runs on any dev box.
#
# Usage:
#   scripts/prerelease.sh            # run local checks
#   scripts/prerelease.sh --quick    # skip cargo-deny for iteration
#   scripts/prerelease.sh --help

set -euo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
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

usage() { sed -n '2,/^$/p' "$REPO_ROOT/scripts/prerelease.sh" | sed 's/^# \{0,1\}//'; }

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
require_cmd python3 "install Python 3.11 or later"

export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"

# Use the same fixture prefix for the builds and the installed-worker tests.
LEAN_SYSROOT=$(scripts/fixture-lean-prefix.sh)
export LEAN_SYSROOT

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

# clippy --workspace is link-safe (no linking happens) per AGENTS.md.
run_gate "cargo clippy --locked --workspace --all-targets -- -D warnings" \
	cargo clippy --locked --workspace --all-targets -- -D warnings

# rustdoc per member: a broken or private intra-doc link is a warning docs.rs
# renders as dead text, so it fails here instead of shipping.
run_gate "cargo doc --locked -p lean-host-mcp --no-deps (-D warnings)" \
	env RUSTDOCFLAGS="-D warnings" cargo doc --locked -p lean-host-mcp --no-deps
run_gate "cargo doc --locked -p lean-host-mcp-worker --no-deps (-D warnings)" \
	env RUSTDOCFLAGS="-D warnings" cargo doc --locked -p lean-host-mcp-worker --no-deps

# Build and test PER-MEMBER. A build selecting both workspace members
# unifies the `lean-rs-sys` feature set across the
# parent and worker crates, silently re-linking `libleanshared` into the
# parent (AGENTS.md "Build and validation"). That both violates the
# parent ⊥ libleanshared invariant and makes the parent's own test binary
# unrunnable — it references `@rpath/libleanshared.dylib` with no rpath to
# find it. Scoping to `-p` keeps the parent on `lean-rs-sys` metadata-only,
# so its tests run on any machine without the dylib on the loader path.
run_gate "cargo build --locked -p lean-host-mcp --all-targets" \
	cargo build --locked -p lean-host-mcp --all-targets

run_gate "cargo build --locked -p lean-host-mcp-worker --all-targets" \
	cargo build --locked -p lean-host-mcp-worker --all-targets

run_gate "cargo test --locked -p lean-host-mcp-worker --all-targets" \
	cargo test --locked -p lean-host-mcp-worker --all-targets
run_gate "cargo test --locked -p lean-host-mcp --all-targets (installed stdio worker)" \
	scripts/test-installed-stdio.sh --all-targets

run_gate "Parent release link check" scripts/check-parent-link.sh

if command -v rg >/dev/null 2>&1; then
	run_gate "Lake-native check" scripts/check-lake-native.sh
else
	skip_gate "Lake-native check (rg not installed)"
fi

# -- Local formatting and Markdown lint checks (beyond CI) ------------------

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

# -- Dependency hygiene (cargo-deny also runs in CI; shear is local) ---------

if [[ "$QUICK" == 1 ]]; then
	skip_gate "cargo deny --locked check (--quick)"
elif command -v cargo-deny >/dev/null 2>&1; then
	run_gate "cargo deny --locked check" cargo deny --locked check
else
	skip_gate "cargo deny --locked check (cargo-deny not installed)"
fi

if command -v cargo-shear >/dev/null 2>&1; then
	run_gate "cargo shear --locked" cargo shear --locked
else
	skip_gate "cargo shear --locked (cargo-shear not installed)"
fi

# -- summary ----------------------------------------------------------------

printf '\n%s====== Pre-release summary ======%s\n' "$BOLD" "$RESET"
printf '\npassed (%d):\n' "${#PASSED[@]}"
if ((${#PASSED[@]} > 0)); then
	for name in "${PASSED[@]}"; do printf '  %s✓%s %s\n' "$GREEN" "$RESET" "$name"; done
fi

if ((${#SKIPPED[@]} > 0)); then
	printf '\nskipped (%d):\n' "${#SKIPPED[@]}"
	for name in "${SKIPPED[@]}"; do printf '  %s-%s %s\n' "$YELLOW" "$RESET" "$name"; done
fi

if ((${#FAILED[@]} > 0)); then
	printf '\n%sfailed (%d):%s\n' "$RED" "${#FAILED[@]}" "$RESET"
	for name in "${FAILED[@]}"; do printf '  %s✗%s %s\n' "$RED" "$RESET" "$name"; done
	exit 1
fi

if ((${#SKIPPED[@]} > 0)); then
	printf '\n%sAll completed checks passed.%s\n' "$GREEN" "$RESET"
else
	printf '\n%sAll checks passed.%s\n' "$GREEN" "$RESET"
fi
