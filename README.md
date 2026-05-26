# lean-host-mcp

A Model Context Protocol server that hosts Lean 4 in a supervised worker child via the `lean-rs-worker-parent` +
`lean-rs-worker-child` crate pair. The parent process owns a shims-only `LeanWorkerHostHandle`; the worker child owns
the `LeanRuntime` and bundled host-shim capabilities. The parent does **not** link `libleanshared`, which lets one
running `lean-host-mcp` serve projects on different Lean toolchains: each toolchain has its own pre-built worker binary
installed under `~/.local/share/lean-host-mcp/workers/<toolchain>/`. Tool calls run as Meta and kernel operations inside
that child rather than as messages to an external LSP. A wedged tactic or runaway typeclass loop kills the child; the
supervisor restarts it instead of taking down the MCP server. That's the difference from `lean-lsp-mcp`.

Fourteen tools are exposed: six session-backed Lean operations (`elaborate`, `kernel_check`, `infer_type`, `whnf`,
`is_def_eq`, `hover_by_name`), a filesystem sweep (`project_scan`), three SQLite-indexed lookups (`find_symbol`,
`find_lemma`, `outline`), three cursor-driven queries (`goal_at_position`, `type_at_position`, `references_of_name`),
and a file-scoped diagnostics query (`file_diagnostics`). Per-tool request and result schemas live in
[`docs/tool-catalog.md`](docs/tool-catalog.md); internal layering in [`docs/architecture.md`](docs/architecture.md).

## Prerequisite: any built Lake project

A consumer project needs only:

- A `lakefile.lean` or `lakefile.toml`.
- A successful `lake build` for the modules the tools will import, so their `.olean` files exist on the search path.
  The default `lake build` with no target is the usual setup step.

The `lean-rs-host` shim that exports the 28 mandatory + 6 optional `lean_rs_host_*` symbols is **bundled inside
`lean-rs-host` itself**—a vendored Lake package the host builds once per toolchain (at first session open) and loads
without touching the consumer project's `:shared` facet. Consumer projects do not declare it, link it, or `@[export]`
its symbols. Lake's `lake-manifest.json` lists every transitive package the project depends on; the server walks it to
find each package's `.lake/packages/<name>/.lake/build/lib/lean` and adds those directories to the importer's search
path. Projects with mathlib or other dependencies work without extra configuration as long as those dependencies' own
`lake build` has run. For mathlib, the standard `lake exe cache get` pulls precompiled oleans; use the equivalent setup
for other dependencies. `fixtures/lean/` in this repo is a demo target the test suite hammers; it isn't a template you
must mirror.

## Build and run

```sh
# 1. Install the parent binary. Build per-member, never `cargo build --workspace`
#    (workspace builds unify feature flags and silently link libleanshared
#    into the parent).
cd /path/to/lean-host-mcp
cargo install --path crates/lean-host-mcp

# 2. Install worker binaries for your local Lean toolchains.
#    With no mode flag, install-worker scans ~/.elan/toolchains and builds
#    any missing workers; each target lands under
#    ~/.local/share/lean-host-mcp/workers/<id>/.
lean-host-mcp install-worker
lean-host-mcp install-worker --toolchain v4.30.0-rc2
lean-host-mcp install-worker --list           # see what's installed

# 3a. Zero-config: launch from inside (or anywhere under) any built Lake
#     project. The toolchain pin is read from `lean-toolchain`, the project
#     root from `lakefile.{lean,toml}`. Tool calls own their own `imports`;
#     no project umbrella is imported unless a call passes it explicitly.
cd /path/to/your/lake/project
lake build && lean-host-mcp

# 3b. Explicit: pin the default project. Equivalent to setting
#     LEAN_HOST_MCP_PROJECT.
lean-host-mcp --lake-root /path/to/your/lake/project
```

Project resolution chain (used by every tool call that does not pass its own `project="..."` argument):

1. `LEAN_HOST_MCP_PROJECT` (or `--lake-root`)
2. Walk upward from the server's cwd looking for `lakefile.{toml,lean}`
3. `~/.config/lean-host-mcp/config.toml` `primary_project = "/abs/path"`

Per call, an MCP client can pass `project="/abs/path/to/other/lake/root"` to route that single call elsewhere—useful
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

**One server, multiple toolchains.** The server picks the worker binary for each project from its `lean-toolchain` pin
and sets `LEAN_SYSROOT` invisibly per spawn (via `LeanWorkerChild::for_toolchain`). A single `lean-host-mcp` process can
serve projects on every toolchain you have installed a worker for (`lean-host-mcp install-worker --toolchain <id>`). You
do not need to set `LEAN_SYSROOT` in the MCP client config.

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

`freshness.imports` is the import vector supplied for that call. An empty array means the caller asked for no extra
imports beyond the worker's base environment.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors.
MCP errors are reserved for infrastructure failures: the worker thread died, the runtime failed to initialise, the Lake
project is unusable.

## Capability shims and the position-tool cluster

`goal_at_position`, `type_at_position`, `references_of_name`, and `file_diagnostics` depend on an optional
`lean_rs_host_process_module_with_info_tree` shim. A worker whose bundled shims do not expose it answers
`{ "status": "unsupported" }` per call; the tools never raise. Files whose header imports modules the server's open env
doesn't have are still processed; missing imports surface as an envelope warning (single-file tools) or a result sidebar
(`references_of_name`). Files using Lean 4's module-system header syntax, including `module`, `public import`,
`import all`, and `meta import`, are supported. A header that doesn't parse short-circuits to `header_parse_failed`.

Unlike an external LSP process, the host can still start when unrelated project modules are broken. Calls whose imports
avoid the broken module continue to work, and `file_diagnostics` on the broken file reports Lean diagnostics instead of
a bootstrap failure.

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

`lean-host-mcp` 0.1.0 targets `lean-rs-worker-parent` / `lean-rs-worker-child` 0.1.14 (which transitively pin `lean-rs`
/ `lean-rs-host` 0.1.14). The server inherits whichever Lean toolchain each consumer Lake project pins, provided it sits
inside the `lean-rs` support window declared by
[`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). Bumping the supported
toolchain is a `lean-rs` change first, then a version bump here.

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
