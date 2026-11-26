# shellcheck shell=bash
# Sourced by the installed-worker test scripts. Set LEAN_RS_CHECKOUT to an SDK checkout to build the
# writer, shims, worker and host from it instead of crates.io. Nothing here is committed or installed
# outside the caller's scratch directory.
#
# local_sdk_enable SCRATCH points every later Cargo call in this shell at the checkout. It writes a
# private CARGO_HOME whose config patches crates.io, links the real registry and Git caches into it,
# and saves Cargo.lock, which the patch makes Cargo rewrite. Call local_sdk_restore SCRATCH on exit.
# Without LEAN_RS_CHECKOUT both do nothing and the registry SDK is used.

LOCAL_SDK_ROOT=

local_sdk_enable() {
    [[ -n ${LEAN_RS_CHECKOUT:-} ]] || return 0
    local scratch=$1 real home entry manifest name
    LOCAL_SDK_ROOT=$(CDPATH='' cd -- "$LEAN_RS_CHECKOUT" && pwd -P)
    real=$(CDPATH='' cd -- "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)
    home="$scratch/cargo-home"
    mkdir -p "$home"
    for entry in registry git; do
        if [[ -e $real/$entry ]]; then
            ln -s "$real/$entry" "$home/$entry"
        fi
    done
    {
        printf '[patch.crates-io]\n'
        for manifest in "$LOCAL_SDK_ROOT"/crates/*/Cargo.toml; do
            name=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["name"])' "$manifest")
            printf '%s = { path = "%s" }\n' "$name" "${manifest%/Cargo.toml}"
        done
    } >"$home/config.toml"
    cp Cargo.lock "$scratch/Cargo.lock.registry"
    export CARGO_HOME="$home"
}

local_sdk_restore() {
    if [[ -f $1/Cargo.lock.registry ]]; then
        cp "$1/Cargo.lock.registry" Cargo.lock
    fi
}

# Run a Cargo subcommand with --locked, except against a checkout: there the patch must rewrite
# Cargo.lock, and local_sdk_restore puts the committed one back.
local_sdk_cargo() {
    local subcommand=$1
    shift
    if [[ -n $LOCAL_SDK_ROOT ]]; then
        cargo "$subcommand" "$@"
    else
        cargo "$subcommand" --locked "$@"
    fi
}

# Install the SDK writer into ROOT/bin: from the checkout when one is set, else the pinned release.
# Print its version so the log shows which writer the tests ran.
local_sdk_install_writer() {
    local root=$1 version=$2
    if [[ -n $LOCAL_SDK_ROOT ]]; then
        cargo install --debug --locked --path "$LOCAL_SDK_ROOT/crates/lean-rs-writer" --root "$root"
    else
        cargo install --debug --locked --version "=$version" --root "$root" lean-rs-writer
    fi
    "$root/bin/lean-rs-writer" --version
}

# Run PARENT install-worker ARGS, taking the shims from the checkout when one is set.
local_sdk_install_worker() {
    local parent=$1
    shift
    if [[ -n $LOCAL_SDK_ROOT ]]; then
        "$parent" install-worker "$@" --shim-source "$LOCAL_SDK_ROOT/crates/lean-rs-host"
    else
        "$parent" install-worker "$@"
    fi
}

# Fail when Cargo kept a registry SDK crate in the graph of MANIFEST. Cargo ignores a patch whose
# version does not satisfy the pin, so a checkout at another version would otherwise test the
# release silently.
local_sdk_check() {
    [[ -n $LOCAL_SDK_ROOT ]] || return 0
    cargo metadata --format-version 1 --manifest-path "$1" | python3 -c '
import json, sys
registry = sorted({p["name"] for p in json.load(sys.stdin)["packages"]
                   if p["name"].startswith(("lean-rs", "lean-toolchain")) and p["source"]})
if registry:
    sys.exit("LEAN_RS_CHECKOUT was not used for " + ", ".join(registry)
             + "; its version must satisfy the host pin")
'
}
