# lean-host-mcp

A Model Context Protocol server that hosts Lean 4 in a supervised worker child via the `lean-rs-worker-parent` +
`lean-rs-worker-child` crate pair. The parent process owns a `LeanWorkerCapability`; the worker child owns the
`LeanRuntime` and `LeanCapabilities` dylib. The parent does **not** link `libleanshared`, which is how a single
running `lean-host-mcp` can serve projects on different Lean toolchains: each toolchain has its own pre-built worker
binary installed under `~/.local/share/lean-host-mcp/workers/<toolchain>/`. Tool calls run as Meta and kernel operations inside
that child rather than as messages to an external LSP — and when a tactic wedges or a typeclass loop runs away, the
supervisor restarts the child instead of taking down the MCP server. That's the difference from `lean-lsp-mcp`.

Fourteen tools are exposed: six session-backed Lean operations (`elaborate`, `kernel_check`, `infer_type`, `whnf`,
`is_def_eq`, `hover_by_name`), a filesystem sweep (`project_scan`), three SQLite-indexed lookups (`find_symbol`,
`find_lemma`, `outline`), three cursor-driven queries (`goal_at_position`, `type_at_position`, `references_of_name`),
and a file-scoped diagnostics query (`file_diagnostics`). Per-tool request and result schemas live in
[`docs/tool-catalog.md`](docs/tool-catalog.md); internal layering in [`docs/architecture.md`](docs/architecture.md).

## Prerequisite: a Lake project linking the host shim

`lean-rs-host` loads a Lean capability dylib that exports 28 mandatory and 6 optional `lean_rs_host_*` symbols. This
crate does not ship one. The server resolves a Lake project whose `lakefile.{toml,lean}` already wires those interop
shims; the `fixtures/lean/` directory in this repo is the reference
template and doubles as a starting point you can adapt.

## Build and run

```sh
# 1. Build the reference shim (one-off).
cd /path/to/lean-host-mcp/fixtures/lean
lake build

# 2. Install the parent binary. Build per-member, never `cargo build --workspace`
#    (workspace builds unify feature flags and silently link libleanshared
#    into the parent).
cd /path/to/lean-host-mcp
cargo install --path crates/lean-host-mcp

# 3. Install one worker binary per Lean toolchain you want to serve.
#    --auto scans ~/.elan/toolchains and builds any missing ones; the
#    target lands under ~/.local/share/lean-host-mcp/workers/<id>/.
lean-host-mcp install-worker --toolchain v4.30.0-rc2
lean-host-mcp install-worker --auto
lean-host-mcp install-worker --list           # see what's installed

# 4a. Zero-config: launch from inside (or anywhere under) a Lake project
#     that exposes the shims. The package, library, umbrella import, and
#     worker toolchain are all auto-discovered.
cd /path/to/lean-host-mcp/fixtures/lean
lean-host-mcp

# 4b. Explicit: pin the default project. Equivalent to setting
#     LEAN_HOST_MCP_PROJECT.
lean-host-mcp --lake-root /path/to/lean-host-mcp/fixtures/lean
```

Project resolution chain (used by every tool call that does not pass its own `project="..."` argument):

1. `LEAN_HOST_MCP_PROJECT` (or `--lake-root`)
2. Walk upward from the server's cwd looking for `lakefile.{toml,lean}`
3. `~/.config/lean-host-mcp/config.toml` `primary_project = "/abs/path"`

Per call, an MCP client can pass `project="/abs/path/to/other/lake/root"` to route that single call elsewhere — useful
when a single client surveys several projects.

Environment vars:

| Variable | Purpose | Default |
| --- | --- | --- |
| `LEAN_HOST_MCP_PROJECT` | Default Lake root for calls without a `project=` argument. | unset |
| `LEAN_HOST_MCP_CACHE_DIR` | SQLite declaration-index store. | `$XDG_CACHE_HOME/lean-host-mcp` |
| `LEAN_HOST_MCP_MAX_PROJECTS` | Max [`LeanProject`]s kept resident; oldest is evicted on overflow. | `4` |
| `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Window after which an unused project is reaped. `0` disables. | `600` |

## Wiring into Claude Code

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/abs/path/to/lean-host-mcp/target/release/lean-host-mcp"
      // No args needed when the client launches the server inside the
      // target Lake project; otherwise pass `--lake-root /abs/path`.
    }
  }
}
```

## Response envelope

Every tool returns the same outer shape; only `result` varies.

```jsonc
{
  "result":   { /* tool-specific */ },
  "freshness": {
    "project_root":   "/abs/path",
    "project_hash":   "sha256-hex of lake-manifest.json",
    "imports":        ["Mod.A", "..."],
    "session_id":     "uuid",          // stable identity of the project actor; changes only on re-spawn (LRU/idle/manifest)
    "lean_toolchain": "leanprover/lean4:v4.29.1"
  },
  "warnings":     ["..."],     // omitted when empty
  "next_actions": ["..."]      // omitted when empty
}
```

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors.
MCP errors are reserved for infrastructure failures: the worker thread died, the runtime failed to initialise, the Lake
project is unusable.

## Capability shims and the position-tool cluster

`goal_at_position`, `type_at_position`, `references_of_name`, and `file_diagnostics` depend on an optional
`lean_rs_host_process_module_with_info_tree` shim. A capability dylib built without it answers
`{ "status": "unsupported" }` per call; the tools never raise. Files whose header imports modules the server's open env
doesn't have are still processed; missing imports surface as an envelope warning (single-file tools) or a result sidebar
(`references_of_name`). A header that doesn't parse short-circuits to `header_parse_failed`.

## Build, test, lint

```sh
cargo build -p lean-host-mcp                          # parent only
cargo build -p lean-host-mcp-worker                   # worker only (links libleanshared)
cargo clippy --workspace --all-targets -- -D warnings # safe; clippy doesn't link
cargo test -p lean-host-mcp                           # unit tests; no Lean fixture required
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
    cargo test -p lean-host-mcp --test e2e -- --ignored   # opt-in end-to-end
```

Build per-member (`-p <name>`); avoid `cargo build --workspace`, which unifies the `lean-rs-sys` feature set across
members and silently links `libleanshared` into the parent. The invariant is asserted by:

```sh
! otool -L target/release/lean-host-mcp | grep -q libleanshared    # macOS
! ldd  target/release/lean-host-mcp | grep -q libleanshared        # Linux
```

## Versions

`lean-host-mcp` 0.1.0 targets `lean-rs-worker-parent` / `lean-rs-worker-child` 0.1.8 (which transitively pin
`lean-rs` / `lean-rs-host` 0.1.8). The MCP
server inherits whichever toolchain the consumer's Lake project pins, provided it sits inside the `lean-rs` support
window declared by [`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). Bumping
the supported toolchain is a `lean-rs` change first, then a version bump here.

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
