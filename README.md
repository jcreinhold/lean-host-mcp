# lean-host-mcp

Model Context Protocol server that hosts Lean 4 in-process via
[`lean-rs`](https://crates.io/crates/lean-rs). The "host" in the name signals
what differentiates it from `lean-lsp-mcp`: this server owns a `LeanRuntime`
and a `LeanCapabilities` dylib directly rather than wrapping an external LSP
process.

## Status — v0.1

Seven tools that work today against the published `lean-rs` 0.1.x:

| Tool | What it does |
| --- | --- |
| `elaborate` | Type-check a Lean term against the project environment; return structured diagnostics on failure. |
| `kernel_check` | Run a full elaborate + kernel-check on a declaration source; return `Checked` / `Rejected` / `Unavailable` / `Unsupported` plus diagnostics. |
| `infer_type` | `Meta.inferType` on a term, with bounded heartbeats. |
| `whnf` | `Meta.whnf` on a term. |
| `is_def_eq` | `Meta.isDefEq` on two terms (default transparency). |
| `hover_by_name` | Look up kind and source range for a fully-qualified Lean name. |
| `project_scan` | Filesystem regex sweep for `sorry`, `admit`, `axiom`, `set_option`, or a custom pattern. |

Explicitly **not** in v0.1 (deferred to v0.2+):

- **Declaration enumeration** (`find_symbol`, `find_lemma`, `outline`).
  The published `lean-rs` 0.1.x has no `LeanName → String` rendering on the
  Rust side; `list_declarations` returns opaque handles. A new `@[export]`
  shim in `lean-rs` lands this in v0.2.
- **Pretty-printed types**. `hover_by_name`'s `type_signature` field is
  always `None` for the same reason — `LeanExpr` is opaque across the
  worker channel boundary. v0.2.
- **Position-based queries** (`goal`, `hover`, `references` at cursor) —
  new shims in `lean-rs`. v0.2.
- `try_tactics`, `unfold_at`, `explain_simp` — depend on the position API.
- `edit` / `replace_proof` — depend on a re-elaborate-after-edit shim.
- `lean-rs-worker` process isolation — v0.3.

See `docs/version-matrix.md` for the supported `lean-rs` / Lean toolchain
matrix.

## Prerequisite — the shim contract

`lean-rs-host` loads a Lean capability dylib that exports 26 mandatory + 4
optional `lean_rs_host_*` symbols. v0.1 of this server does not bundle a
self-contained shim crate. You point `--lake-root` at a Lake project whose
`lakefile.lean` already wires up the `lean-rs-host` interop shims (see
[`lean-rs/fixtures/lean/`](https://github.com/jcreinhold/lean-rs/tree/main/fixtures/lean)
for the canonical template).

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

The server reads `LEAN_HOST_MCP_LAKE_ROOT`, `LEAN_HOST_MCP_PACKAGE`,
`LEAN_HOST_MCP_LIBRARY`, `LEAN_HOST_MCP_IMPORTS`, and
`LEAN_HOST_MCP_CACHE_DIR` from the environment if you prefer not to repeat
flags.

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

This is the only shape every tool shares. See `docs/tool-catalog.md` for
per-tool schemas.

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
