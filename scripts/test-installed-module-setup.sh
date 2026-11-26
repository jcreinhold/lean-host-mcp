#!/usr/bin/env bash
# Package the parent and worker, build and install the pair from the archives, and prepare semantic search with it.
# With --release, also check the packaged parent's link set and run the stdio suite against the packaged worker.
# Set LEAN_RS_CHECKOUT to build the pair, writer and shims from an SDK checkout; release checks leave it unset.
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
sdk=$(python3 -c 'import tomllib; [v]=[p["version"] for p in tomllib.load(open("Cargo.lock","rb"))["package"] if p["name"]=="lean-toolchain"]; print(v)')
mkdir -p "$CARGO_TARGET_DIR"
scratch=$(mktemp -d "$CARGO_TARGET_DIR/module-setup.XXXXXX")
trap 'local_sdk_restore "$scratch"; rm -rf -- "$scratch"' EXIT
export TMPDIR="$scratch/tmp" LEAN_RS_WORKER_OWNERS_DIRECTORY="$scratch/owners"
export LEAN_HOST_MCP_SEMANTIC_CACHE_DIR="$scratch/semantic-cache"
mkdir -p "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$TMPDIR" "$scratch/cache" "$scratch/server-owners"
# Verify the actual archives below with their published dependency locks.
cargo package --locked --workspace --no-verify
local_sdk_enable "$scratch"
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
python3 - "$pair" "$version" "$LOCAL_SDK_ROOT" <<'PY_LOCK'
import pathlib
import sys
import tomllib

pair, version, local_sdk = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
host_names = {"lean-host-mcp", "lean-host-mcp-worker"}

def is_sdk(name):
    return bool(local_sdk) and name.startswith(("lean-rs", "lean-toolchain"))

def read_lock(directory):
    with (directory / "Cargo.lock").open("rb") as stream:
        return tomllib.load(stream)["package"]

def registry_identities(entries):
    return {
        (p["name"], p["version"], p.get("source"), p.get("checksum"))
        for p in entries if p["name"] not in host_names and not is_sdk(p["name"])
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
local_sdk_install_writer "$scratch/tools" "$sdk"
local_sdk_install_worker "$parent" --toolchain "$id" --workers-dir "$scratch/workers" \
    --source-dir "$worker_source" --standalone-writer "$scratch/tools/bin/lean-rs-writer"
# Build the bundled Lean capability under the packaged writer. Cold
# compilation has its own SDK budget.
local_sdk_cargo run --manifest-path "$parent_manifest" --example prepare_semantic_smoke -- \
    "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$toolchain" "$LEAN_SYSROOT" "$parent"
if "$release"; then
    scripts/check-parent-link.sh "$parent"
    LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$scratch/workers" scripts/test-installed-stdio.sh --all-targets
fi
