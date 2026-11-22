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
cargo package --locked --workspace
# Cargo verifies the archives by unpacking them beside the .crate files.
archives="$CARGO_TARGET_DIR/package"
parent_manifest="$archives/lean-host-mcp-$version/Cargo.toml"
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
    --source-dir "$archives/lean-host-mcp-worker-$version" --standalone-writer "$R2_SETUP_ROOT/tools/bin/lean-rs-writer"
python3 tests/r2-launch-input/module_setup.py
if "$managed_release"; then
    # Keep the exact packaged executable while Cargo builds the checkout's
    # test targets in the same artifact directory.
    packaged_parent="$scratch/lean-host-mcp"
    cp "$R2_SETUP_PARENT" "$packaged_parent"
    scripts/check-parent-link.sh "$packaged_parent"
    LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$R2_SETUP_ROOT/workers" \
        LEAN_HOST_MCP_STDIO_PACKAGED_LAUNCHER="$packaged_parent" \
        scripts/test-installed-stdio.sh --all-targets --features managed-native-store
fi
