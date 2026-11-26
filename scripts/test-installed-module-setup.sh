#!/usr/bin/env bash
# Validate ordinary file-backed endpoints and optionally managed release against the packaged pair.
# Set LEAN_RS_CHECKOUT to build the pair, writer and shims from an SDK checkout; release checks leave it unset.
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."
# shellcheck source=scripts/local-sdk.sh
source scripts/local-sdk.sh
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
trap 'local_sdk_restore "$scratch"; rm -rf -- "$scratch"' EXIT
export R2_SETUP_ROOT="$scratch"
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

[patch.crates-io]
lean-host-mcp-worker = { path = "lean-host-mcp-worker-$version" }
EOF
cp "$pair/lean-host-mcp-$version/Cargo.lock" "$pair/Cargo.lock"
cargo update --offline --manifest-path "$pair/Cargo.toml" -p lean-host-mcp-worker
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
unset LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT
parent_features=()
warm_release=false
if "$managed_release"; then
    parent_features=(--features managed-native-store)
    if python3 -c 'import sys,tomllib; sys.exit("managed-warm-inspections" not in tomllib.load(open(sys.argv[1],"rb"))["features"])' "$parent_manifest"; then
        parent_features=(--features managed-warm-inspections)
        warm_release=true
    fi
fi
local_sdk_cargo build --manifest-path "$parent_manifest" --bin lean-host-mcp ${parent_features[@]+"${parent_features[@]}"}
local_sdk_check "$pair/Cargo.toml"
# The source worker installer expects Cargo's native profile directories.
built_parent="$CARGO_TARGET_DIR/debug/lean-host-mcp"
if [[ ! -x $built_parent ]]; then
    printf 'Expected native parent binary at %s\n' "$built_parent" >&2
    exit 1
fi
export R2_SETUP_PARENT="$scratch/shipping-lean-host-mcp"
cp "$built_parent" "$R2_SETUP_PARENT"
local_sdk_install_writer "$R2_SETUP_ROOT/tools" "$sdk"
local_sdk_install_worker "$R2_SETUP_PARENT" --toolchain "$id" --workers-dir "$R2_SETUP_ROOT/workers" \
    --source-dir "$worker_source" --standalone-writer "$R2_SETUP_ROOT/tools/bin/lean-rs-writer"
# Build the bundled Lean capability under the same packaged writer before the
# RPC smoke. Cold compilation has its own existing SDK budget.
local_sdk_cargo run --manifest-path "$parent_manifest" --example prepare_semantic_smoke \
    ${parent_features[@]+"${parent_features[@]}"} -- \
    "$LEAN_HOST_MCP_SEMANTIC_CACHE_DIR" "$toolchain" "$LEAN_SYSROOT" "$R2_SETUP_PARENT"
python3 tests/r2-launch-input/module_setup.py
if "$managed_release"; then
    scripts/check-parent-link.sh "$R2_SETUP_PARENT"
    if "$warm_release" && [[ -z ${LEAN_HOST_MCP_PREVIOUS_COLD_HOST:-} ]]; then
        printf 'Skipping warm inspection: set LEAN_HOST_MCP_PREVIOUS_COLD_HOST to a previous host binary.\n' >&2
        warm_release=false
    fi
    if "$warm_release"; then
        warm_scratch=$(mktemp -d "${RUNNER_TEMP:-$CARGO_TARGET_DIR}/w.XXXXXX")
        python3 tests/r2-launch-input/prepare_generation.py --warm-inspection \
            --host "$R2_SETUP_PARENT" --writer "$R2_SETUP_ROOT/tools/bin/lean-rs-writer" \
            --sysroot "$LEAN_SYSROOT" --scratch "$warm_scratch" --workers "$R2_SETUP_ROOT/workers"
        rm -rf -- "$warm_scratch"
    fi
    export LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR="$R2_SETUP_ROOT/workers"
    export LEAN_HOST_MCP_STDIO_PACKAGED_WRITER="$R2_SETUP_ROOT/tools/bin/lean-rs-writer"
    LEAN_HOST_MCP_STDIO_PACKAGED_LAUNCHER="$R2_SETUP_PARENT" \
        scripts/test-installed-stdio.sh --all-targets "${parent_features[@]}"

    # Only the injected rename fault uses this separately compiled executable.
    LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT=1 \
        local_sdk_cargo build --manifest-path "$parent_manifest" --bin lean-host-mcp "${parent_features[@]}"
    fault_parent="$scratch/fault-only-lean-host-mcp"
    cp "$built_parent" "$fault_parent"
    LEAN_HOST_MCP_STDIO_PACKAGED_LAUNCHER="$fault_parent" \
        scripts/test-installed-stdio.sh "${parent_features[@]}" --test stdio_lifecycle -- \
        --ignored --exact launcher_regressions::launcher_uncertain_publication_reconciles_without_another_generation
fi
