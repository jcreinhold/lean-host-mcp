#!/usr/bin/env bash
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."

toolchain=$(cat fixtures/lean/lean-toolchain)
prefix=${LEAN_SYSROOT:-}
if [[ -z $prefix ]]; then
    if ! prefix=$(elan run "$toolchain" lean --print-prefix); then
        printf 'Install the fixture toolchain with: elan toolchain install %s\n' "$toolchain" >&2
        exit 1
    fi
fi

case "$(uname -s)" in
Darwin) shared=libleanshared.dylib ;;
Linux) shared=libleanshared.so ;;
*)
    printf 'The Lean fixture requires macOS or Linux.\n' >&2
    exit 1
    ;;
esac

if [[ $prefix != /* || ! -f "$prefix/include/lean/lean.h" || ! -f "$prefix/lib/lean/$shared" ||
    ! -x "$prefix/bin/lean" || ! -x "$prefix/bin/lake" ]]; then
    printf 'Expected an absolute, complete Lean prefix for %s, got: %s\n' "$toolchain" "$prefix" >&2
    exit 1
fi

printf '%s\n' "$prefix"
