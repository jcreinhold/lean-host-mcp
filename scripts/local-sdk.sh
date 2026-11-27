# shellcheck shell=bash
# Sourced by the installed-worker test scripts. Set LEAN_RS_CHECKOUT to an SDK checkout, or
# LEAN_SEMANTIC_SEARCH_CHECKOUT to a semantic search checkout, to build the writer, worker and host
# from it instead of crates.io. Nothing here is committed or installed outside the caller's scratch
# directory.
#
# local_sdk_enable SCRATCH points every later Cargo call in this shell at the checkouts. It writes a
# private CARGO_HOME whose config patches crates.io, links the real registry and Git caches into it,
# and saves Cargo.lock, which the patch makes Cargo rewrite. Call local_sdk_restore SCRATCH on exit.
# The patch lives in CARGO_HOME because Clippy and nextest do not see `cargo --config` patches.
# Without a checkout both do nothing and the registry crates are used.

LOCAL_SDK_ROOT=
# Name prefixes of the patched packages, separated by spaces; empty when nothing is patched.
LOCAL_SDK_PREFIXES=

local_sdk_enable() {
    [[ -n ${LEAN_RS_CHECKOUT:-}${LEAN_SEMANTIC_SEARCH_CHECKOUT:-} ]] || return 0
    local scratch=$1 real home entry checkout manifest name
    local checkouts=()
    if [[ -n ${LEAN_RS_CHECKOUT:-} ]]; then
        LOCAL_SDK_ROOT=$(CDPATH='' cd -- "$LEAN_RS_CHECKOUT" && pwd -P)
        checkouts+=("$LOCAL_SDK_ROOT")
        LOCAL_SDK_PREFIXES="lean-rs lean-toolchain"
    fi
    if [[ -n ${LEAN_SEMANTIC_SEARCH_CHECKOUT:-} ]]; then
        checkouts+=("$(CDPATH='' cd -- "$LEAN_SEMANTIC_SEARCH_CHECKOUT" && pwd -P)")
        LOCAL_SDK_PREFIXES="${LOCAL_SDK_PREFIXES:+$LOCAL_SDK_PREFIXES }lean-semantic-search"
    fi
    real=$(CDPATH='' cd -- "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)
    home="$scratch/cargo-home"
    mkdir -p "$home"
    for entry in registry git; do
        if [[ -e $real/$entry ]]; then
            ln -sfn "$real/$entry" "$home/$entry"
        fi
    done
    {
        printf '[patch.crates-io]\n'
        for checkout in "${checkouts[@]}"; do
            for manifest in "$checkout"/crates/*/Cargo.toml; do
                name=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["name"])' "$manifest")
                printf '%s = { path = "%s" }\n' "$name" "${manifest%/Cargo.toml}"
            done
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
    if [[ -n $LOCAL_SDK_PREFIXES ]]; then
        cargo "$subcommand" "$@"
    else
        cargo "$subcommand" --locked "$@"
    fi
}

# Install the SDK writer into ROOT/bin: from the SDK checkout when one is set, else the pinned
# release. Print its version so the log shows which writer the tests ran.
local_sdk_install_writer() {
    local root=$1 version=$2
    if [[ -n $LOCAL_SDK_ROOT ]]; then
        cargo install --debug --locked --path "$LOCAL_SDK_ROOT/crates/lean-rs-writer" --root "$root"
    else
        cargo install --debug --locked --version "=$version" --root "$root" lean-rs-writer
    fi
    "$root/bin/lean-rs-writer" --version
}

# Fail when Cargo kept a registry copy of a patched crate in the graph of MANIFEST. Cargo ignores a
# patch whose version does not satisfy the pin, so a checkout at another version would otherwise
# test the release silently.
local_sdk_check() {
    [[ -n $LOCAL_SDK_PREFIXES ]] || return 0
    cargo metadata --format-version 1 --manifest-path "$1" | python3 -c '
import json, sys
prefixes = tuple(sys.argv[1].split())
registry = sorted({p["name"] for p in json.load(sys.stdin)["packages"]
                   if p["name"].startswith(prefixes) and p["source"]})
if registry:
    sys.exit("no checkout was used for " + ", ".join(registry)
             + "; its version must satisfy the host pin")
' "$LOCAL_SDK_PREFIXES"
}
