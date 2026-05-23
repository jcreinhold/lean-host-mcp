# lean-host-mcp

A Model Context Protocol server that hosts Lean 4 in-process through [`lean-rs`](https://crates.io/crates/lean-rs). It
owns a `LeanRuntime` and a `LeanCapabilities` dylib directly, so tool calls run as in-process Meta and kernel operations
rather than as messages to an external LSP. That's the difference from `lean-lsp-mcp`.

Fourteen tools are exposed: six session-backed Lean operations (`elaborate`, `kernel_check`, `infer_type`, `whnf`,
`is_def_eq`, `hover_by_name`), a filesystem sweep (`project_scan`), three SQLite-indexed lookups (`find_symbol`,
`find_lemma`, `outline`), three cursor-driven queries (`goal_at_position`, `type_at_position`, `references_of_name`),
and a file-scoped diagnostics query (`file_diagnostics`). Per-tool request and result schemas live in
[`docs/tool-catalog.md`](docs/tool-catalog.md); internal layering in [`docs/architecture.md`](docs/architecture.md).

## Prerequisite: a Lake project linking the host shim

`lean-rs-host` loads a Lean capability dylib that exports 28 mandatory and 6 optional `lean_rs_host_*` symbols. This
crate does not ship one. Point `--lake-root` at a Lake project whose `lakefile.lean` already wires those interop shims;
[`lean-rs/fixtures/lean/`](https://github.com/jcreinhold/lean-rs/tree/main/fixtures/lean) is the reference template and
doubles as a starting point you can adapt.

## Build and run

```sh
# 1. Build the reference shim (one-off).
cd /path/to/lean-rs/fixtures/lean
lake build

# 2. Build the MCP server.
cd /path/to/lean-host-mcp
cargo build --release

# 3. Launch, pointed at the built Lake project.
./target/release/lean-host-mcp \
    --lake-root /path/to/lean-rs/fixtures/lean \
    --package lean_rs_fixture \
    --library LeanRsFixture \
    --imports LeanRsFixture.Handles
```

Every flag also reads from an environment variable: `LEAN_HOST_MCP_LAKE_ROOT`, `LEAN_HOST_MCP_PACKAGE`,
`LEAN_HOST_MCP_LIBRARY`, `LEAN_HOST_MCP_IMPORTS`, `LEAN_HOST_MCP_CACHE_DIR`.

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

## Response envelope

Every tool returns the same outer shape; only `result` varies.

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

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors.
MCP errors are reserved for infrastructure failures: the worker thread died, the runtime failed to initialise, the Lake
project is unusable.

## Capability shims and the position-tool cluster

`goal_at_position`, `type_at_position`, `references_of_name`, and `file_diagnostics` depend on an optional
`lean_rs_host_process_module_with_info_tree` shim. A capability dylib built without it answers
`{ "status": "unsupported" }` per call; the tools never raise. Files whose header imports modules the server's open
env doesn't have are still processed; missing imports surface as an envelope warning (single-file tools) or a result
sidebar (`references_of_name`). A header that doesn't parse short-circuits to `header_parse_failed`.

## Build, test, lint

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo test                                # unit tests; no Lean fixture required
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
    cargo test --test e2e -- --ignored    # opt-in end-to-end
```

## Versions

`lean-host-mcp` 0.1.0 targets `lean-rs` / `lean-rs-host` 0.1.4, which pins Lean toolchain
`leanprover/lean4:v4.30.0-rc2`. Bumping the supported toolchain is a `lean-rs` change first, then a version bump here.
The MCP server inherits whichever toolchain the consumer's Lake project pins, provided it sits inside the `lean-rs`
support window declared by [`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain).

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
