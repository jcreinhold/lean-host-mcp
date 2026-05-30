#!/usr/bin/env bash
# .claude/hooks/format.sh — PostToolUse formatter (Edit|Write|MultiEdit).
#
# Reformats the single file Claude just edited so it always matches CI's
# fmt gate (`cargo fmt --check`, taplo, mdwright). Reads the hook JSON on
# stdin, dispatches on extension, and ALWAYS exits 0 — formatting must
# never block or undo an edit. Formatter stderr is suppressed: a file
# mid-edit may be syntactically incomplete, and a scary rustfmt error is
# noise, not signal.
#
# Each formatter is best-effort and skipped if its tool is absent, so the
# hook is safe on machines without taplo/mdwright installed.
set -euo pipefail

# The hook contract puts the edited path at .tool_input.file_path. jq is a
# documented prerequisite for this repo; bail quietly if it or the field
# is missing.
command -v jq >/dev/null 2>&1 || exit 0
input="$(cat)"
file="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')"
[ -n "$file" ] && [ -f "$file" ] || exit 0

case "$file" in
*.rs)
	command -v rustfmt >/dev/null 2>&1 && rustfmt "$file" >/dev/null 2>&1 || true
	;;
*.toml)
	command -v taplo >/dev/null 2>&1 && taplo fmt "$file" >/dev/null 2>&1 || true
	;;
*.json)
	# `npx --yes` so a missing prettier is fetched non-interactively rather
	# than hanging on npx's install prompt; skipped entirely if npx is absent.
	command -v npx >/dev/null 2>&1 && npx --yes prettier --write "$file" >/dev/null 2>&1 || true
	;;
*.md)
	# Format all Markdown — no carveouts. CLAUDE.md/AGENTS.md (a symlink to
	# CLAUDE.md) and docs under .claude/ are formatted like any other file.
	command -v mdwright >/dev/null 2>&1 && mdwright fmt "$file" >/dev/null 2>&1 || true
	;;
esac

exit 0
