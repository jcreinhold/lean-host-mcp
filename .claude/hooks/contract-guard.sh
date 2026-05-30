#!/usr/bin/env bash
# .claude/hooks/contract-guard.sh — PostToolUse advisory (Edit|Write|MultiEdit).
#
# Surfaces two lean-host-mcp invariants that CI catches late (or not at
# all) as *non-blocking* reminders. The edit has already happened; we only
# print guidance to stderr and exit 2 so the text is returned to Claude to
# act on. We never undo anything.
#
#   1. stdout is the MCP transport (CLAUDE.md / main.rs). Stdio is the
#      default transport and stdout carries JSON-RPC frames; diagnostics go
#      to stderr via `tracing`. A `println!`/`print!` in library/worker
#      code corrupts the protocol stream. The repo has ZERO such calls in
#      src today — keep it that way.
#   2. The parent never links libleanshared (CLAUDE.md "Multi-toolchain
#      dispatch"). Only crates/lean-host-mcp-worker may depend on the
#      Lean-runtime crates. Adding lean-rs / lean-rs-host / lean-rs-sys /
#      lean-rs-worker-child to the PARENT manifest silently re-links the
#      dylib and breaks multi-toolchain dispatch. (lean-rs-worker-parent is
#      the shims-only handle and is expected there.)
#
# KNOWN TRADEOFF: both checks are plain greps, so check 1 can false-positive
# on doc examples that legitimately show `println!`. They are advisory only,
# so a rare nudge is cheap; an AST-aware check is the wrong altitude for a
# hook. If check 1 gets noisy, scope it to non-`tests/` paths.
set -euo pipefail

command -v jq >/dev/null 2>&1 || exit 0
input="$(cat)"
file="$(printf '%s' "$input" | jq -r '.tool_input.file_path // empty')"
[ -n "$file" ] && [ -f "$file" ] || exit 0

msgs=()

# 1. stdout cleanliness — no print macros under a crate's src/, EXCEPT the
#    parent's install-worker CLI path (src/cli/*), whose subcommands write
#    plain tables to stdout and never share the process with the MCP server.
#    Everywhere else, user-facing output routes through the rmcp transport
#    (stdout) or tracing (stderr); raw print macros are never correct.
#    (eprintln!/eprint! are fine and don't match — the \b before println!
#    fails inside "eprintln!".)
case "$file" in
*/crates/*/src/cli/*) : ;;
*/crates/*/src/*.rs)
	if grep -Eq '\b(println!|print!)[[:space:]]*\(' "$file"; then
		msgs+=("• $file uses println!/print!. stdout is reserved for the stdio MCP transport's JSON-RPC frames (CLAUDE.md; src/main.rs). Route diagnostics to stderr via tracing; never print to stdout from src/. (The install-worker CLI under src/cli/ is the only allowed exception.)")
	fi
	;;
esac

# 2. Parent ⊥ libleanshared — guard the parent crate's manifest only.
case "$file" in
*/crates/lean-host-mcp/Cargo.toml)
	if grep -Eq '^[[:space:]]*(lean-rs-host|lean-rs-sys|lean-rs-worker-child|lean-rs)([[:space:]]|=|\.|")' "$file"; then
		msgs+=("• $file (the PARENT manifest) gained a Lean-runtime dependency. Per CLAUDE.md only crates/lean-host-mcp-worker may link libleanshared; the parent must stay free of it. Keep lean-rs / lean-rs-host / lean-rs-sys / lean-rs-worker-child in the worker crate. Verify with: ! otool -L target/release/lean-host-mcp | grep -q libleanshared (macOS) / ! ldd … (Linux). Build per-member with -p, never --workspace.")
	fi
	;;
esac

if [ "${#msgs[@]}" -gt 0 ]; then
	printf 'contract-guard:\n' >&2
	printf '%s\n' "${msgs[@]}" >&2
	exit 2
fi

exit 0
