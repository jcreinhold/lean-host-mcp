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
`loaded` mode. `lean_runtime` reports the server's build, settings and worker activity. Any other tool takes an optional
`request_id`: the server records the call before it starts, runs it at most once and keeps its result, so a retry
returns that result. `lean_run` starts such a request and replies at once; `lean_get` reads its state and result.

`lean_build` runs Lake in the project: Lake targets, or a recipe of Lake steps from the project's config. Checks wait
while it runs and then see what it built. `lean_imports` lists each file's whole transitive import closure from
`lake setup-file`, and builds nothing.

Semantic responses contain `data`, structured `errors`, and project/session `trust`. Lean-domain failures remain in
`data`; recoverable runtime failures appear in `errors`. See the [tool catalog](docs/tool-catalog.md) for request and
response schemas, proof-position selectors, artifact freshness, and
[named requests](docs/tool-catalog.md#named-requests).

## Quick start

Install the server, then install what one Lake project needs. `install` needs Rust on `PATH` and the project's Lean
toolchain installed through elan:

```sh
cargo install --locked lean-host-mcp   # from a checkout: cargo install --locked --path crates/lean-host-mcp
lean-host-mcp install --lake-root /path/to/project
```

`install` reads `/path/to/project/lean-toolchain` and installs under `/path/to/project/.local/lean-host-mcp/`, which Git
ignores: a copy of the server in `bin/`; the worker for that toolchain in `workers/`; the server's private store in
`store/`; and `config.toml`, which records their absolute paths under `[install]`. `--dir` moves `bin/`, `workers/` and
`store/`; without it, `LEAN_HOST_MCP_WORKERS_DIR` and `LEAN_HOST_MCP_STORE_DIR` move the worker directory and the store
for both `install` and the server. `install` builds the semantic search runtime into the store, which also holds the
server's lock and process records, and `config.toml` records the elan home, so neither `install` nor the server keeps
state under `HOME` (see [operations](docs/operations.md)). `install` takes the store as a server does, so stop the
server before you rerun it. Rerun `install` after a toolchain bump or a `lean-host-mcp` upgrade. A server installed from
a checkout builds the worker from that workspace; `--source-dir` selects another checkout. Build each workspace member
with `-p`; `cargo build --workspace` would link `libleanshared` into the parent and break multi-toolchain dispatch. See
[server installation](docs/operations.md#installing-the-server) and
[development commands](docs/operations.md#build-test-lint).

Build the project, then serve it with the installed copy:

```sh
cd /path/to/project
lake build
.local/lean-host-mcp/bin/lean-host-mcp --lake-root /path/to/project
```

One server serves a checkout: a second server on the same store exits and names the first. While the server runs, build
through `lean_build` rather than a raw `lake build`. The server orders only the Lake and Lean runs it starts, so a raw
build can rewrite `.olean` files while the server reads them, which is outside its contract.

The project needs a `lakefile.lean` or `lakefile.toml`, a `lean-toolchain` pin, and built `.olean` files for requested
imports. The server reads dependency paths from `lake-manifest.json`. Project dependencies follow their usual build or
precompiled-artifact setup; the host shims and semantic-search runtime are supplied by the server's dependencies.
Consumer projects do not declare or link those packages. [`fixtures/lean/`](fixtures/lean/README.md) is the test
example.

## Connecting an MCP client

Stdio is the default. Configure a client that launches the installed copy by its absolute path:

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/abs/path/to/project/.local/lean-host-mcp/bin/lean-host-mcp",
      "args": ["--lake-root", "/abs/path/to/project"]
    }
  }
}
```

`--lake-root` names the one project the server runs Lean for, and the directory whose config files it reads. Only one
server runs per project store: a second one exits and names the pid of the first. The server selects the project's
worker and sets `LEAN_SYSROOT` for each worker it starts; the client needs no toolchain environment override.

For a native local client that connects by URL, start Streamable HTTP:

```sh
/path/to/project/.local/lean-host-mcp/bin/lean-host-mcp serve --lake-root /path/to/project --bind 127.0.0.1:8765
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

A server serves one Lake project: `--lake-root` or `LEAN_HOST_MCP_PROJECT`. A call may name it with
`project: "/abs/path"`; a call that names another root fails, and so does every call when the server has no root. The
server reads two config files there: the tracked `lean-host-mcp.toml`, then the ignored
`.local/lean-host-mcp/config.toml` that `install` writes, whose keys win. It reads no config file from its working
directory or your home directory. Run `lean-host-mcp config init --lake-root PROJECT` to create a documented tracked
file. The [operations guide](docs/operations.md#configuration-file) covers precedence, pool sizing, deadlines, memory
budgets, response presentation, the request table, and the `[build]` and `[recipes]` tables that `lean_build` reads (see
[builds](docs/operations.md#builds)). By default the project uses one worker; `broker.max_workers_per_project` allows
concurrent calls within the `broker.max_workers` slots.

## Versions

| Component | Current source requirement |
| --- | --- |
| `lean-host-mcp` | 0.18.0 |
| `lean-rs` stack and `lean-toolchain` | 0.12.0 |
| `lean-semantic-search` stack | 0.9.0 |
| Worker protocol | 14 |
| Supported Lean window | 4.30.0 through 4.36.0 |

Install the parent and workers together; protocol 14 rejects older workers before requests. After an upgrade from 0.17,
a plain `install` replaces the worker (see [upgrading](docs/operations.md#upgrading-to-018)). Registry installations may
differ from the source requirements shown here. The development fixture uses Lean 4.36.0; a local install does not
qualify the full toolchain/platform release matrix.

The exact supported releases come from `lean_toolchain::SUPPORTED_TOOLCHAINS`, re-exported from
[`lean-rs-abi`](https://github.com/jcreinhold/lean-rs/blob/main/crates/lean-rs-abi/src/supported.rs). Numbered pins
outside that table are rejected with the supported window and nearest version; `install-worker` also refuses them.
Widening support starts in `lean-rs`, followed by dependency and worker updates here.

## Documentation

- [Tool catalog](docs/tool-catalog.md): public tool contracts and proof workflow.
- [Operations](docs/operations.md): configuration, installation, runtime failures, and development checks.
- [Architecture](docs/architecture.md): contributor boundaries, worker ownership, caching, and how one server owns a
  checkout.
- [Reference indices](docs/architecture.md#reference-indices): project-scope reference lookup, schema, and performance
  evidence.
- [Changelog](CHANGELOG.md): release history.

## License

MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
