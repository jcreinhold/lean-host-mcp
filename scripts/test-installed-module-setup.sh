#!/usr/bin/env bash
# Validate ordinary file-backed endpoints and optionally managed release against the packaged pair.
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."
managed_release=false
if [[ ${1:-} == --managed-release ]]; then
    managed_release=true
    shift
fi
if [[ $# != 0 ]]; then
    printf 'Usage: %s [--managed-release]\n' "$0" >&2
    exit 2
fi
toolchain=$(cat fixtures/lean/lean-toolchain)
id=${toolchain##*:}
LEAN_SYSROOT=$(scripts/fixture-lean-prefix.sh)
export LEAN_SYSROOT
CARGO_TARGET_DIR=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
if [[ $CARGO_TARGET_DIR != /* ]]; then
    printf 'Cargo target directory must be absolute: %s\n' "$CARGO_TARGET_DIR" >&2
    exit 1
fi
export CARGO_TARGET_DIR
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
sdk=$(python3 -c 'import tomllib; [v]=[p["version"] for p in tomllib.load(open("Cargo.lock","rb"))["package"] if p["name"]=="lean-toolchain"]; print(v)')
mkdir -p "$CARGO_TARGET_DIR"
scratch=$(mktemp -d "$CARGO_TARGET_DIR/module-setup.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
export R2_SETUP_ROOT="$scratch"
export TMPDIR="$scratch/tmp" LEAN_RS_WORKER_OWNERS_DIRECTORY="$scratch/owners"
export LEAN_HOST_MCP_SEMANTIC_CACHE_DIR="$scratch/semantic-cache"
mkdir -p "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$TMPDIR" "$scratch/cache" "$scratch/server-owners"
# Verify the actual archives below with their published dependency locks.
cargo package --locked --workspace --no-verify
archives="$CARGO_TARGET_DIR/package"
pair="$scratch/archives"
mkdir "$pair"
for package in lean-host-mcp lean-host-mcp-worker; do
    tar -xzf "$archives/$package-$version.crate" -C "$pair"
done
parent_manifest="$pair/lean-host-mcp-$version/Cargo.toml"
worker_source="$pair/lean-host-mcp-worker-$version"
cat >"$pair/Cargo.toml" <<EOF
[workspace]
members = ["lean-host-mcp-$version", "lean-host-mcp-worker-$version"]
resolver = "3"

[patch.crates-io]
lean-host-mcp-worker = { path = "lean-host-mcp-worker-$version" }
EOF
cp "$pair/lean-host-mcp-$version/Cargo.lock" "$pair/Cargo.lock"
cargo update --offline --manifest-path "$pair/Cargo.toml" -p lean-host-mcp-worker
python3 - "$pair" "$version" <<'PY_LOCK'
import pathlib
import sys
import tomllib

pair, version = pathlib.Path(sys.argv[1]), sys.argv[2]
host_names = {"lean-host-mcp", "lean-host-mcp-worker"}

def read_lock(directory):
    with (directory / "Cargo.lock").open("rb") as stream:
        return tomllib.load(stream)["package"]

def registry_identities(entries):
    return {
        (p["name"], p["version"], p.get("source"), p.get("checksum"))
        for p in entries if p["name"] not in host_names
    }

published_locks = set().union(*(
    registry_identities(read_lock(pair / f"{name}-{version}")) for name in host_names
))
actual = read_lock(pair)
sources = [(p["name"], p["version"], p.get("source")) for p in actual if p["name"] in host_names]
if sorted(sources) != [(name, version, None) for name in sorted(host_names)]:
    raise SystemExit(f"expected the exact unpacked candidate pair, found {sources!r}")
identities = registry_identities(actual)
if (not identities <= published_locks
        or any(source != "registry+https://github.com/rust-lang/crates.io-index" or not checksum
               for _, _, source, checksum in identities)):
    raise SystemExit("candidate pairing changed external registry identities")
PY_LOCK
if "$managed_release"; then
    LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT=1 cargo build --locked --manifest-path "$parent_manifest" \
        --bin lean-host-mcp --features managed-native-store
else
    cargo build --locked --manifest-path "$parent_manifest" --bin lean-host-mcp
fi
# The source worker installer expects Cargo's native profile directories.
export R2_SETUP_PARENT="$CARGO_TARGET_DIR/debug/lean-host-mcp"
if [[ ! -x $R2_SETUP_PARENT ]]; then
    printf 'Expected native parent binary at %s\n' "$R2_SETUP_PARENT" >&2
    exit 1
fi
cargo install --debug --locked --root "$R2_SETUP_ROOT/tools" --version "=$sdk" lean-rs-writer
"$R2_SETUP_PARENT" install-worker --toolchain "$id" --workers-dir "$R2_SETUP_ROOT/workers" \
    --source-dir "$worker_source" --standalone-writer "$R2_SETUP_ROOT/tools/bin/lean-rs-writer"
python3 tests/r2-launch-input/module_setup.py
if "$managed_release"; then
    # Keep the exact packaged executable while Cargo builds the checkout's
    # test targets in the same artifact directory.
    packaged_parent="$scratch/lean-host-mcp"
    cp "$R2_SETUP_PARENT" "$packaged_parent"
    scripts/check-parent-link.sh "$packaged_parent"
    LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$R2_SETUP_ROOT/workers" \
        LEAN_HOST_MCP_STDIO_PACKAGED_LAUNCHER="$packaged_parent" \
        LEAN_HOST_MCP_STDIO_PACKAGED_WRITER="$R2_SETUP_ROOT/tools/bin/lean-rs-writer" \
        scripts/test-installed-stdio.sh --all-targets --features managed-native-store
fi
