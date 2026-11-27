#!/usr/bin/env bash
# Package the parent and worker, build and install the pair from the archives, and prepare semantic search with it.
# With --release, also check the packaged parent's link set and run the stdio suite against the packaged worker.
# Set LEAN_RS_CHECKOUT or LEAN_SEMANTIC_SEARCH_CHECKOUT to build the pair from those checkouts; release
# checks leave both unset.
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."
# shellcheck source=scripts/local-sdk.sh
source scripts/local-sdk.sh
release=false
if [[ ${1:-} == --release ]]; then
    release=true
    shift
fi
if [[ $# != 0 ]]; then
    printf 'Usage: %s [--release]\n' "$0" >&2
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
mkdir -p "$CARGO_TARGET_DIR"
scratch=$(mktemp -d "$CARGO_TARGET_DIR/module-setup.XXXXXX")
trap 'local_sdk_restore "$scratch"; rm -rf -- "$scratch" ${sockets:+"$sockets"}' EXIT
# Worker sockets: the default is under HOME, and a path in the scratch is too
# long for macOS.
sockets=$(mktemp -d "$(cd -- "$CARGO_TARGET_DIR" && pwd -P)/s.XXXXXX")
export LEAN_HOST_MCP_SOCKET_DIR="$sockets"
export TMPDIR="$scratch/tmp" LEAN_RS_WORKER_OWNERS_DIRECTORY="$scratch/owners"
export LEAN_HOST_MCP_SEMANTIC_CACHE_DIR="$scratch/semantic-cache"
mkdir -p "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$TMPDIR" "$scratch/cache" "$scratch/server-owners"
# Verify the actual archives below with the dependency locks they ship. A checkout's crates are not
# published yet, so the archives can only be made against it.
local_sdk_enable "$scratch"
local_sdk_cargo package --workspace --no-verify
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
EOF
cp "$pair/lean-host-mcp-$version/Cargo.lock" "$pair/Cargo.lock"
# The parent does not depend on the worker, so its lock lacks the worker's
# entries. Add them and leave every locked package as it is.
cargo update --offline --manifest-path "$pair/Cargo.toml" --workspace
python3 - "$pair" "$version" "$LOCAL_SDK_PREFIXES" <<'PY_LOCK'
import pathlib
import sys
import tomllib

pair, version, patched = pathlib.Path(sys.argv[1]), sys.argv[2], tuple(sys.argv[3].split())
host_names = {"lean-host-mcp", "lean-host-mcp-worker"}

def is_patched(name):
    return name.startswith(patched)

def read_lock(directory):
    with (directory / "Cargo.lock").open("rb") as stream:
        return tomllib.load(stream)["package"]

def registry_identities(entries):
    return {
        (p["name"], p["version"], p.get("source"), p.get("checksum"))
        for p in entries if p["name"] not in host_names and not is_patched(p["name"])
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
local_sdk_cargo build --manifest-path "$parent_manifest" --bin lean-host-mcp
local_sdk_check "$pair/Cargo.toml"
# The source worker installer expects Cargo's native profile directories.
built_parent="$CARGO_TARGET_DIR/debug/lean-host-mcp"
if [[ ! -x $built_parent ]]; then
    printf 'Expected native parent binary at %s\n' "$built_parent" >&2
    exit 1
fi
parent="$scratch/shipping-lean-host-mcp"
cp "$built_parent" "$parent"
# The packaged parent serves the SDK writer for the worker's shims and the
# bundled Lean capability. Cold compilation has its own SDK budget.
"$parent" install-worker --toolchain "$id" --workers-dir "$scratch/workers" \
    --source-dir "$worker_source"
local_sdk_cargo run --manifest-path "$parent_manifest" --example prepare_semantic_smoke -- \
    "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$toolchain" "$LEAN_SYSROOT" "$parent"
if "$release"; then
    scripts/check-parent-link.sh "$parent"
    LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$scratch/workers" scripts/test-installed-stdio.sh --all-targets
fi
