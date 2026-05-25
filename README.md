# lean-host-mcp

A Model Context Protocol server that hosts Lean 4 in a supervised worker child via
[`lean-rs-worker`](https://crates.io/crates/lean-rs-worker). The parent process owns a `LeanWorkerCapability`; the
worker child owns the `LeanRuntime` and `LeanCapabilities` dylib. Tool calls run as Meta and kernel operations inside
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
shims; [`lean-rs/fixtures/lean/`](https://github.com/jcreinhold/lean-rs/tree/main/fixtures/lean) is the reference
template and doubles as a starting point you can adapt.

## Build and run

```sh
# 1. Build the reference shim (one-off).
cd /path/to/lean-rs/fixtures/lean
lake build

# 2. Build the MCP server. `--bins` builds both the server and its sibling
#    worker child (`lean-host-mcp-worker`), which the parent resolves via
#    `LeanWorkerChild::sibling`; both end up next to each other in
#    `target/release/`.
cd /path/to/lean-host-mcp
cargo build --release --bins

# 3a. Zero-config: launch from inside (or anywhere under) a Lake project
#     that exposes the shims. The package, library, and umbrella import
#     are auto-discovered from the lakefile.
cd /path/to/lean-rs/fixtures/lean
/path/to/target/release/lean-host-mcp

# 3b. Explicit: pin the default project. Equivalent to setting
#     LEAN_HOST_MCP_PROJECT.
./target/release/lean-host-mcp --lake-root /path/to/lean-rs/fixtures/lean
```

Project resolution chain (used by every tool call that does not pass its own `project="..."` argument):

1. `LEAN_HOST_MCP_PROJECT` (or `--lake-root`)
2. Walk upward from the server's cwd looking for `lakefile.{toml,lean}`
3. `~/.config/lean-host-mcp/config.toml` `primary_project = "/abs/path"`

Per call, an MCP client can pass `project="/abs/path/to/other/lake/root"` to route that single call elsewhere — useful
when a single client surveys several projects.

Environment vars: `LEAN_HOST_MCP_PROJECT`, `LEAN_HOST_MCP_CACHE_DIR`.

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
    "session_id":     "uuid",
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
cargo build
cargo clippy --all-targets -- -D warnings
cargo test                                # unit tests; no Lean fixture required
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
    cargo test --test e2e -- --ignored    # opt-in end-to-end
```

## Versions

`lean-host-mcp` 0.1.0 targets `lean-rs-worker` 0.1.7 (which transitively pins `lean-rs` / `lean-rs-host` 0.1.7). The MCP
server inherits whichever toolchain the consumer's Lake project pins, provided it sits inside the `lean-rs` support
window declared by [`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). Bumping
the supported toolchain is a `lean-rs` change first, then a version bump here.

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
