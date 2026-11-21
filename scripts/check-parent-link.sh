#!/usr/bin/env bash
# Build and inspect the parent separately so worker features cannot affect its link set.
set -euo pipefail
CDPATH='' cd -- "$(dirname -- "$0")/.."

binary=$(
    cargo build --locked --release -p lean-host-mcp --bin lean-host-mcp \
        --message-format=json-render-diagnostics |
        python3 -c '
import json
import sys

executables = []
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-artifact" and message.get("executable"):
        target = message["target"]
        if target["name"] == "lean-host-mcp" and target["kind"] == ["bin"]:
            executables.append(message["executable"])
if len(executables) != 1:
    sys.exit("Cargo did not report exactly one lean-host-mcp executable")
print(executables[0])
'
)
if [[ ! -f "$binary" || ! -x "$binary" ]]; then
    printf 'Expected a runnable parent executable at %s\n' "$binary" >&2
    exit 1
fi

os=$(uname -s)
case "$os" in
Darwin) dependencies=$(otool -L "$binary") ;;
Linux) dependencies=$(ldd "$binary") ;;
*)
    printf 'Parent link inspection is unsupported on %s\n' "$os" >&2
    exit 1
    ;;
esac

lean_library='(^|[[:space:]/])libleanshared\.(so(\.[0-9]+)*|dylib)([[:space:]]|$)'
if [[ "$dependencies" =~ $lean_library ]]; then
    printf 'The parent executable links libleanshared: %s\n' "$binary" >&2
    exit 1
fi
