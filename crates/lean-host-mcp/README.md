# lean-host-mcp

A Model Context Protocol server for Lean 4 proof context, lemma search, declaration inspection, tactic trials, and
kernel verification. Lean runs inside a supervised worker child, separate from the server process. One server can host
projects pinned to different Lean toolchains.

## Install and run

```sh
cargo install --locked lean-host-mcp
lean-host-mcp install --lake-root /path/to/project
cd /path/to/project
lake build
.local/lean-host-mcp/bin/lean-host-mcp --lake-root /path/to/project
```

`install` requires Rust on `PATH` and the project's Lean toolchain installed through elan. It builds the worker for the
project's `lean-toolchain` and puts it, a copy of the server, the server's store and a local config file under the
project's `.local/lean-host-mcp/`, which Git ignores. It never writes a tracked file. Stdio is the default transport;
`--bind 127.0.0.1:8765` selects loopback Streamable HTTP at `/mcp`.

## Tools

| Tool | Purpose |
| --- | --- |
| `lean_context` | Proof goals, locals, and position context. |
| `lean_trial` | In-memory proof-step and command experiments. |
| `lean_verify` | Declaration verification for explicit, file, or module targets. |
| `lean_lookup` | Declaration inspection, inventory, proof search, and references. |
| `lean_status` | Project/toolchain status and file diagnostics. |
| `lean_compatibility` | Compiled requirements or actual loaded-worker compatibility evidence. |
| `lean_runtime` | Worker activity and expiring idle-pool leases. |
| `lean_run` | Start a named request, recorded before it runs and run at most once, and reply at once. |
| `lean_get` | A named request's state, and its stored result once it has ended. |

Tools do not apply source edits automatically. Lean commands, tactics, and import initializers can perform IO with the
server user's privileges. Use trusted local clients and projects; the worker provides crash isolation, not an OS
sandbox.

## Documentation

See [setup and client configuration](https://github.com/jcreinhold/lean-host-mcp/blob/main/README.md),
[tool contracts](https://github.com/jcreinhold/lean-host-mcp/blob/main/docs/tool-catalog.md),
[operations](https://github.com/jcreinhold/lean-host-mcp/blob/main/docs/operations.md), and
[architecture](https://github.com/jcreinhold/lean-host-mcp/blob/main/docs/architecture.md).

## License

MIT OR Apache-2.0.
