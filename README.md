# lean-host-mcp

A Model Context Protocol server that gives an AI agent access to a Lean 4 project's elaborator and kernel. Agents can
read proof context, find lemmas, inspect declarations, try tactics, verify declarations, and find semantic references.
Tools read source and elaborate in memory without applying source edits automatically.

Lean runs inside a supervised worker child rather than through an external LSP. A wedged tactic, runaway typeclass loop,
or worker crash is isolated from the server. Each project's `lean-toolchain` pin selects a matching worker, so one
server can host several Lean toolchains.

Use trusted local clients and projects. Lean commands, tactics, and import initializers can perform IO with the server
user's privileges. The worker provides crash isolation, not an OS sandbox; `#eval IO.FS.writeFile ...` can write files.

## Tools

Five semantic tool families cover the proof workflow:

| Tool | Modes and purpose |
| --- | --- |
| `lean_context` | `proof_position`: goals and local context at a declaration proof position |
| `lean_lookup` | `declaration`, `declarations`, `proof_search`, `references`: inspect, search, and navigate |
| `lean_trial` | `proof_step`, `command`: try tactics or Lean commands in memory |
| `lean_verify` | Verify explicit, file-wide, or module-wide declaration target groups |
| `lean_status` | `project`, `file_diagnostics`: project status or current-source diagnostics |

`lean_compatibility` describes compiled expectations with `mode: "pure_v1"`, or checks an actual worker in the default
`loaded` mode. `lean_runtime` observes worker activity and leases an idle worker pool. `lean_request` prepares and
submits retained requests, retrieves results, and requests cancellation without requiring MCP Tasks negotiation.

Semantic responses contain `data`, structured `errors`, and project/session `trust`. Lean-domain failures remain in
`data`; recoverable runtime failures appear in `errors`. See the [tool catalog](docs/tool-catalog.md) for request and
response schemas, proof-position selectors, artifact freshness, and the
[retained request lifecycle](docs/tool-catalog.md#prepared-retained-lifecycle-v3).

## Quick start

Install the published server, then build a worker for each supported Lean toolchain you use. Worker installation needs
Rust on `PATH` and the matching Lean toolchain installed through elan:

```sh
cargo install lean-host-mcp --locked
lean-host-mcp install-worker                      # install missing or stale workers
lean-host-mcp install-worker --toolchain v4.35.0  # install one toolchain
lean-host-mcp install-worker --list               # inspect build/runtime/protocol status
```

Workers live under `~/.local/share/lean-host-mcp/workers/<toolchain>/`. From a source checkout, install the server with
`cargo install --path crates/lean-host-mcp --locked`; `install-worker` then builds the worker from that workspace.
`--source-dir` selects another checkout. Build each workspace member with `-p`; `cargo build --workspace` would link
`libleanshared` into the parent and break multi-toolchain dispatch. See
[development commands](docs/operations.md#build-test-lint).

Run the server in a built Lake project:

```sh
cd /path/to/your/lake/project
lake build
lean-host-mcp
```

The project needs a `lakefile.lean` or `lakefile.toml`, a `lean-toolchain` pin, and built `.olean` files for requested
imports. The server reads dependency paths from `lake-manifest.json`. Project dependencies follow their usual build or
precompiled-artifact setup; the host shims and semantic-search runtime are supplied by the server's dependencies.
Consumer projects do not declare or link those packages. [`fixtures/lean/`](fixtures/lean/README.md) is the test
example.

## Connecting an MCP client

Stdio is the default. Configure a client that launches a command with the installed executable's absolute path:

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/abs/path/to/lean-host-mcp",
      "args": ["--lake-root", "/abs/path/to/lake/project"]
    }
  }
}
```

Omit `args` when the client launches the server inside the target Lake project. The server selects each project's worker
and sets `LEAN_SYSROOT` per spawn; the client needs no toolchain environment override.

For a native local client that connects by URL, start Streamable HTTP:

```sh
lean-host-mcp serve --lake-root /path/to/lake/project --bind 127.0.0.1:8765
```

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

A process serves one transport. `--bind` or `LEAN_HOST_MCP_BIND` selects HTTP; `--http-path` or
`LEAN_HOST_MCP_HTTP_PATH` changes its default `/mcp` route and requires a bind. HTTP binds are loopback-only
(`127.0.0.1` or `::1`), with Host validation and no built-in authentication or TLS. Every request with an `Origin`
header is rejected with HTTP 403; browser access is unsupported.

## Project resolution and configuration

A per-call `project: "/abs/path"` selects that call's Lake root. When omitted, resolution follows:

1. `--lake-root` or `LEAN_HOST_MCP_PROJECT`.
2. The nearest `lakefile.lean` or `lakefile.toml` above the server's working directory.
3. `primary_project` in the discovered configuration file.

Run `lean-host-mcp config init` to create a documented configuration file. The
[operations guide](docs/operations.md#configuration-file) covers discovery and precedence, pool sizing, deadlines,
memory budgets, response presentation, and durable request journals. By default a project uses one worker; configuring
`broker.max_workers_per_project` allows concurrent calls in the same project within the global `broker.max_workers` cap.

## Versions

| Component | Current source requirement |
| --- | --- |
| `lean-host-mcp` | 0.16.13 |
| `lean-rs` stack and `lean-toolchain` | 0.10.5 |
| `lean-semantic-search` stack | 0.7.9 |
| Worker protocol | 13 |
| Supported Lean window | 4.30.0 through 4.36.0 |

Install the parent and workers together; protocol 13 rejects older workers before requests. Registry installations may
differ from the source requirements shown here. The development fixture uses Lean 4.36.0; a local install does
not qualify the full toolchain/platform release matrix.

The exact supported releases come from `lean_toolchain::SUPPORTED_TOOLCHAINS`, re-exported from
[`lean-rs-abi`](https://github.com/jcreinhold/lean-rs/blob/main/crates/lean-rs-abi/src/supported.rs). Numbered pins
outside that table are rejected with the supported window and nearest version; `install-worker` also refuses them.
Widening support starts in `lean-rs`, followed by dependency and worker updates here.

## Documentation

- [Tool catalog](docs/tool-catalog.md): public tool contracts and proof workflow.
- [Operations](docs/operations.md): configuration, installation, runtime failures, and development checks.
- [Architecture](docs/architecture.md): contributor boundaries, worker ownership, caching, and artifact custody.
- [Reference indices](docs/architecture.md#reference-indices): project-scope reference lookup, schema, and performance
  evidence.
- [Changelog](CHANGELOG.md): release history.

## License

MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
