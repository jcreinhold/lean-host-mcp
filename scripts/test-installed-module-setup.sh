#!/usr/bin/env bash
# Validate both ordinary file-backed public endpoints against the packaged pair.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')
export R2_SETUP_ROOT=$(mktemp -d "$CARGO_TARGET_DIR/module-setup.XXXXXX")
export TMPDIR="$R2_SETUP_ROOT/tmp" LEAN_RS_WORKER_OWNERS_DIRECTORY="$R2_SETUP_ROOT/owners"
export LEAN_HOST_MCP_SEMANTIC_CACHE_DIR="$R2_SETUP_ROOT/semantic-cache"
mkdir -p "$TMPDIR" "$R2_SETUP_ROOT/cache" "$R2_SETUP_ROOT/server-owners"
toolchain=$(cat fixtures/lean/lean-toolchain)
id=${toolchain##*:}
export LEAN_SYSROOT="${LEAN_SYSROOT:-$HOME/.elan/toolchains/leanprover--lean4---$id}"
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
sdk=$(python3 -c 'import tomllib; [v]=[p["version"] for p in tomllib.load(open("Cargo.lock","rb"))["package"] if p["name"]=="lean-toolchain"]; print(v)')
cargo package --locked --workspace
archives="$CARGO_TARGET_DIR/package"
cargo build --locked --manifest-path "$archives/lean-host-mcp-$version/Cargo.toml" --bin lean-host-mcp
export R2_SETUP_PARENT="$CARGO_TARGET_DIR/debug/lean-host-mcp"
cargo install --debug --locked --root "$R2_SETUP_ROOT/tools" --version "=$sdk" lean-rs-writer
"$R2_SETUP_PARENT" install-worker --toolchain "$id" --workers-dir "$R2_SETUP_ROOT/workers" \
    --source-dir "$archives/lean-host-mcp-worker-$version" --standalone-writer "$R2_SETUP_ROOT/tools/bin/lean-rs-writer"
python3 tests/r2-launch-input/module_setup.py
