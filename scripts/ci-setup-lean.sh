#!/usr/bin/env bash
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."

: "${GITHUB_PATH:?This script requires a GitHub Actions runner}"
: "${GITHUB_ENV:?This script requires a GitHub Actions runner}"

toolchain=$(cat fixtures/lean/lean-toolchain)
elan_bin="$HOME/.elan/bin"
export PATH="$elan_bin:$PATH"
if [[ ! -x "$elan_bin/elan" ]]; then
    curl -fsSL https://raw.githubusercontent.com/leanprover/elan/master/elan-init.sh |
        sh -s -- -y --no-modify-path --default-toolchain none
fi
"$elan_bin/elan" toolchain install "$toolchain"
prefix=$(scripts/fixture-lean-prefix.sh)
printf '%s\n' "$elan_bin" >>"$GITHUB_PATH"
printf 'LEAN_SYSROOT=%s\n' "$prefix" >>"$GITHUB_ENV"
