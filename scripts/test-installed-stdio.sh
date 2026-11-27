#!/usr/bin/env bash
# Run the parent tests with a test-owned installed worker, including its verified shims.
# Set LEAN_RS_CHECKOUT or LEAN_SEMANTIC_SEARCH_CHECKOUT to build the worker and host from those
# checkouts; the worker brings the shim sources of the SDK it was built with.
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."
# shellcheck source=scripts/local-sdk.sh
source scripts/local-sdk.sh
toolchain=$(cat fixtures/lean/lean-toolchain)
id=${toolchain##*:}
target=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
if [[ $target != /* ]]; then
    printf 'Cargo target directory must be absolute: %s\n' "$target" >&2
    exit 1
fi
export CARGO_TARGET_DIR="$target"
LEAN_SYSROOT=$(scripts/fixture-lean-prefix.sh)
export LEAN_SYSROOT
mkdir -p "$target"
scratch=$(mktemp -d "$target/stdio-install.XXXXXX")
trap 'local_sdk_restore "$scratch"; rm -rf -- "$scratch"' EXIT
local_sdk_enable "$scratch"
if [[ -z ${LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR:-} ]]; then
    export LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$scratch/workers"
    export LEAN_RS_WORKER_OWNERS_DIRECTORY="$scratch/owners"
    local_sdk_cargo build -p lean-host-mcp --bin lean-host-mcp
    local_sdk_check Cargo.toml
    # The source worker installer expects Cargo's native profile directories.
    parent="$target/debug/lean-host-mcp"
    if [[ ! -x $parent ]]; then
        printf 'Expected native parent binary at %s\n' "$parent" >&2
        exit 1
    fi
    "$parent" install-worker --toolchain "$id" \
        --workers-dir "$scratch/workers" --source-dir "$PWD"
fi
# Never add Lake fixture metadata to a user installation.
target=$(cd -- "$target" && pwd -P)
workers=$(CDPATH='' cd -- "$LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR" && pwd -P)
if [[ $workers/ != "$target"/* ]]; then
    printf 'Expected test installation under Cargo target: %s\n' "$workers" >&2
    exit 1
fi
export LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$workers"
slot="$workers/$id"
generation=$(cat "$slot/current-generation")
[[ $generation =~ ^generation-[[:alnum:]]+$ ]]
project=$(cd -- "$slot/$generation/smoke-project" && pwd -P)
if [[ $project != "$workers"/* ]]; then
    printf 'Expected smoke project under test installation: %s\n' "$project" >&2
    exit 1
fi
export LEAN_HOST_MCP_STDIO_TEST_PROJECT="$project"
[[ -f "$slot/$generation/worker.json" ]]
# The installed smoke project already has a locked, empty import directory.
# Give this private project Lake discovery metadata without building an import.
(
    set -o noclobber
    cd "$LEAN_HOST_MCP_STDIO_TEST_PROJECT"
    [[ -f lakefile.toml ]] || printf 'name = "StdioSmoke"\n' >lakefile.toml
    [[ -f lean-toolchain ]] || printf '%s\n' "$toolchain" >lean-toolchain
    [[ -f lake-manifest.json ]] || "$LEAN_SYSROOT/bin/lake" --no-cache update
)
local_sdk_cargo test -p lean-host-mcp "$@"
