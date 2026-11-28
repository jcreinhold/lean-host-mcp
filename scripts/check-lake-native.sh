#!/usr/bin/env bash
# Fail when shipped code reads Lake's files or answers Lake's questions by
# hand: a path under `.lake`, Lake's layout fields, trace files, a shared
# library named by hand, or two file times compared to decide whether a build
# is current. Lake decides where outputs live and whether they are current, so
# ask Lake instead (docs/architecture.md, "Lake's answers").
#
# Shipped code is each crate's `src/` above `mod tests`, which clippy's
# `items_after_test_module` keeps last, without the test-only `*tests.rs`
# modules, plus the Lean programs the server runs. Tests may build a fake Lake
# layout.
#
# No pattern looks for these, which ask Lake nothing:
# - `lake-manifest.json` by name. The server hashes and stats it to notice a
#   changed dependency set, and parses nothing in it.
# - `.olean` and `.ilean` names. Lean's own lookup rule finds a module's file
#   under an import root that Lake named; Lean owns the release's `lib/lean`,
#   and the server owns the layout of its installed shims.
#
# One exception: `source_newer_than_index` in crates/lean-host-mcp/src/ilean.rs.
# The declaration outline compares a module's source and index times before it
# adds the index's rows, since Lake's own check would cost about 1 s more.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v rg >/dev/null 2>&1; then
	printf 'required command not found: rg (install ripgrep)\n' >&2
	exit 2
fi

PATTERNS=(
	-e '\.lake/'                                                           # a path under .lake
	-e '"\.lake"\s*\)'                                                     # .lake joined onto a path in Rust
	-e '/\s*"\.lake"'                                                      # .lake joined onto a path in Lean
	-e 'packagesDir|lakeDir|buildDir|leanLibDir|nativeLibDir|binDir|irDir' # Lake's layout fields
	-e '\.(trace|nobuild)\b|\.hash"'                                       # trace files
	-e '\.(so|dylib)"'                                                     # a shared library named by hand
	-e '(mtime|modified)\S*\s+[<>]=?\s+\S*(mtime|modified)'                # two file times compared
)

files=()
while IFS= read -r -d '' file; do
	if [[ -f "$file" ]]; then
		files+=("$file")
	fi
done < <(git ls-files -z --cached --others --exclude-standard -- 'crates/*/src/*.rs' 'crates/*/src/*.lean' \
	':(exclude)crates/*/src/*tests.rs')
if ((${#files[@]} == 0)); then
	printf 'found no shipped source to check\n' >&2
	exit 2
fi

# Print each shipped line as FILE:LINE: TEXT, leaving out test modules and the
# exception's body.
shipped="$(awk '
	FNR == 1 { tests = 0; exempt = 0 }
	/^(pub\(crate\) )?mod tests \{$/ { tests = 1 }
	tests { next }
	/^pub\(crate\) fn source_newer_than_index\(/ { exempt = 1 }
	exempt { if ($0 == "}") exempt = 0; next }
	{ print FILENAME ":" FNR ": " $0 }
' "${files[@]}")"

status=0
hits="$(rg "${PATTERNS[@]}" <<<"$shipped")" || status=$?
if ((status == 0)); then
	printf 'shipped code reads Lake files or answers a Lake question; ask Lake instead (see %s):\n%s\n' "$0" "$hits" >&2
	exit 1
elif ((status != 1)); then
	exit "$status"
fi
printf 'shipped code reads no Lake file; only the outline compares index times by hand\n'
