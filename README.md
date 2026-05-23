# lean-host-mcp

Model Context Protocol server that hosts Lean 4 in-process via [`lean-rs`](https://crates.io/crates/lean-rs). The "host"
in the name signals what differentiates it from `lean-lsp-mcp`: this server owns a `LeanRuntime` and a
`LeanCapabilities` dylib directly rather than wrapping an external LSP process.

## Status — v0.1.0 (unreleased)

Thirteen tools against `lean-rs` 0.1.3:

| Tool | What it does |
| --- | --- |
| `elaborate` | Type-check a Lean term against the project environment; return structured diagnostics on failure. |
| `kernel_check` | Run a full elaborate + kernel-check on a declaration source; return `Checked` / `Rejected` / `Unavailable` / `Unsupported` plus diagnostics. |
| `infer_type` | `Meta.inferType` on a term, with bounded heartbeats. Result is pretty-printed via `pp_expr`. |
| `whnf` | `Meta.whnf` on a term. Result is pretty-printed via `pp_expr`. |
| `is_def_eq` | `Meta.isDefEq` on two terms with selectable transparency. |
| `hover_by_name` | Look up kind, source range, and rendered type for a fully-qualified Lean name. |
| `project_scan` | Filesystem regex sweep for `sorry`, `admit`, `axiom`, `set_option`, or a custom pattern. |
| `find_symbol` | Case-insensitive substring search across declaration names; backed by the SQLite index. |
| `find_lemma` | As `find_symbol`, restricted to theorems. |
| `outline` | Name-prefix listing (e.g. everything under `Nat.`). |
| `goal_at_position` | Proof goal at a cursor in a `.lean` file. Backed by a content-hashed `ProcessedFile` cache. |
| `type_at_position` | Type (and expected type, when recorded) of the innermost term at a cursor. |
| `references_of_name` | All binder / use-site occurrences of a fully-qualified Lean name across one or many files. |

The three position tools depend on the optional `lean_rs_host_process_with_info_tree` capability shim. A capability
dylib built without it answers `{ "status": "unsupported" }` cleanly per call — the tools never error.

Explicitly **not** here yet (deferred to v0.3+):

- `try_tactics`, `unfold_at`, `explain_simp` — speculative tactic execution at a position.
- `edit` / `replace_proof` — depend on a re-elaborate-after-edit shim.
- `lean-rs-worker` process isolation — v0.3.

See `docs/version-matrix.md` for the supported `lean-rs` / Lean toolchain matrix.

## Prerequisite — the shim contract

`lean-rs-host` 0.1.3 loads a Lean capability dylib that exports 28 mandatory + 6 optional `lean_rs_host_*` symbols.
This crate does not bundle a self-contained shim. You point `--lake-root` at a Lake project whose `lakefile.lean`
already wires up the `lean-rs-host` interop shims (see
[`lean-rs/fixtures/lean/`](https://github.com/jcreinhold/lean-rs/tree/main/fixtures/lean) for the canonical template).
Capability dylibs built against 0.1.2 must be rebuilt: 0.1.3 added two mandatory and two optional shims for name and
expression rendering.

Wiring a shim into your project is a v0.2 README task. For now:

```sh
# 1. Build the lean-rs in-tree fixture (one-off)
cd /path/to/lean-rs/fixtures/lean
lake build

# 2. Build the MCP server
cd /path/to/lean-host-mcp
cargo build --release

# 3. Point it at the built fixture
./target/release/lean-host-mcp \
    --lake-root /path/to/lean-rs/fixtures/lean \
    --package lean_rs_fixture \
    --library LeanRsFixture \
    --imports LeanRsFixture.Handles
```

## Wiring into Claude Code

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/abs/path/to/lean-host-mcp/target/release/lean-host-mcp",
      "args": [
        "--lake-root", "/abs/path/to/your/lake/project",
        "--imports",   "Your.Main.Module"
      ]
    }
  }
}
```

The server reads `LEAN_HOST_MCP_LAKE_ROOT`, `LEAN_HOST_MCP_PACKAGE`, `LEAN_HOST_MCP_LIBRARY`, `LEAN_HOST_MCP_IMPORTS`,
and `LEAN_HOST_MCP_CACHE_DIR` from the environment if you prefer not to repeat flags.

## Response envelope

Every tool returns:

```jsonc
{
  "result":   { /* tool-specific */ },
  "freshness": {
    "lake_root":      "/abs/path",
    "imports":        ["Mod.A", "..."],
    "session_id":     "uuid",
    "lean_toolchain": "leanprover/lean4:v4.29.1"
  },
  "warnings":     ["..."],     // omitted when empty
  "next_actions": ["..."]      // omitted when empty
}
```

This is the only shape every tool shares. See `docs/tool-catalog.md` for per-tool schemas.

## Build, test

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo test                          # unit tests; no Lean fixture required
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
    cargo test --test e2e -- --ignored   # opt-in end-to-end
```

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
