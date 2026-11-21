#!/usr/bin/env bash
# Run the parent tests with a test-owned installed worker, including its verified shims.
set -euo pipefail
cd "$(dirname "$0")/.."
toolchain=$(cat fixtures/lean/lean-toolchain)
id=${toolchain##*:}
target=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
export CARGO_TARGET_DIR="$target"
export LEAN_SYSROOT="${LEAN_SYSROOT:-$HOME/.elan/toolchains/leanprover--lean4---$id}"
scratch=
if [[ -z ${LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR:-} ]]; then
    mkdir -p "$target"
    scratch=$(mktemp -d "$target/stdio-install.XXXXXX")
    export LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$scratch/workers"
    export LEAN_RS_WORKER_OWNERS_DIRECTORY="$scratch/owners"
    sdk=$(python3 -c 'import tomllib; [version] = [p["version"] for p in tomllib.load(open("Cargo.lock", "rb"))["package"] if p["name"] == "lean-toolchain"]; print(version)')
    cargo install --locked --debug --version "=$sdk" --root "$scratch/tools" lean-rs-writer
    cargo build --locked -p lean-host-mcp --bin lean-host-mcp
    "$target/debug/lean-host-mcp" install-worker --toolchain "$id" \
        --workers-dir "$scratch/workers" --source-dir "$PWD" \
        --standalone-writer "$scratch/tools/bin/lean-rs-writer"
fi
# Never add Lake fixture metadata to a user installation.
workers=$(cd "$LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR" && pwd -P)
case "$workers/" in "$(cd "$target" && pwd -P)"/*) ;; *) echo "expected test installation under Cargo target" >&2; exit 1 ;; esac
slot="$LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR/$id"
generation=$(cat "$slot/current-generation")
[[ $generation =~ ^generation-[[:alnum:]]+$ ]]
export LEAN_HOST_MCP_STDIO_TEST_PROJECT="$slot/$generation/smoke-project"
[[ -d $LEAN_HOST_MCP_STDIO_TEST_PROJECT && -f "$slot/$generation/worker.json" ]]
# The installed Init smoke root already has a locked, empty import directory.
# Give this private project Lake discovery metadata without building an import.
( set -o noclobber
  cd "$LEAN_HOST_MCP_STDIO_TEST_PROJECT"
  [[ -f lakefile.toml ]] || printf 'name = "StdioSmoke"\n' > lakefile.toml
  [[ -f lean-toolchain ]] || printf '%s\n' "$toolchain" > lean-toolchain
  [[ -f lake-manifest.json ]] || "$LEAN_SYSROOT/bin/lake" --no-cache update
)
cargo test --locked -p lean-host-mcp "$@"
if [[ -n ${scratch:-} ]]; then
    rm -rf -- "$scratch"
fi
