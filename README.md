# lean-host-mcp

A Model Context Protocol server that runs one Lean 4 project's elaborator and kernel for AI agents. Many agent sessions
share one server, which orders their checks and builds so no check reads a half-written build. Agents read proof state,
find and inspect declarations, try tactics, verify declarations, find references, and build through Lake. The tools
never edit source.

Lean runs in a supervised worker child, not an LSP, so a stuck tactic or a crash stays out of the server. The project's
`lean-toolchain` pin selects the worker.

Use only trusted clients and projects. Lean commands, tactics and import initializers can do IO with the server user's
rights: the worker isolates crashes, it is not a sandbox. The HTTP transport binds only loopback, has no authentication
or TLS, and refuses every request that carries an `Origin` header.

## Install

`install` needs Rust on `PATH` and the project's Lean toolchain installed through elan. The project needs a
`lakefile.lean` or `lakefile.toml` and a `lean-toolchain` pin; it needs no extra packages or targets.

```sh
cargo install --locked lean-host-mcp      # from a checkout: cargo install --locked --path crates/lean-host-mcp
lean-host-mcp install --lake-root /path/to/project
```

`install` writes only under `/path/to/project/.local/lean-host-mcp/`, which Git ignores: a copy of the server, the
worker for the project's toolchain, the server's private store and a local config file. Run it again after a toolchain
bump or an upgrade of this server. [Operations](docs/operations.md#install) gives the options.

## Example

Build the project once, then point a client at the installed copy. Stdio is the default:

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/path/to/project/.local/lean-host-mcp/bin/lean-host-mcp",
      "args": ["--lake-root", "/path/to/project"]
    }
  }
}
```

For a client that connects by URL, start the server with `--bind 127.0.0.1:8765` and use `http://127.0.0.1:8765/mcp`.

An agent that edits `MyProject/Basic.lean` builds its imports, then checks one theorem and its axioms:

```json
{"name":"lean_build","arguments":{"targets":["MyProject.Basic"]}}
{"name":"lean_verify","arguments":{"targets":[{"kind":"explicit","file":"MyProject/Basic.lean","declarations":["MyProject.add_comm'"]}],"report_axioms":true}}
```

The reply holds a `verification_status` and the axioms for each declaration, and `trust.source_sha256` names the bytes
it checked. While the server runs, build through `lean_build`, never a raw `lake build`: the server orders only the runs
it starts. The [tool catalog](docs/tool-catalog.md) gives every tool and reply.

## Versions

| Component | Version |
| --- | --- |
| `lean-host-mcp` and `lean-host-mcp-worker` | 0.19.1 |
| `lean-rs` and `lean-toolchain` | 0.13.0 |
| `lean-semantic-search` | 0.10.0 |
| Worker protocol | 15 |
| Lean | 4.30.0 through 4.36.0 |

Upgrade the server and its workers together: the server refuses a worker whose protocol differs from its own, and
`install` replaces a worker that another release built ([upgrading](docs/operations.md#upgrading-to-019)). The supported
Lean releases are `lean_toolchain::SUPPORTED_TOOLCHAINS`, from
[`lean-rs-abi`](https://github.com/jcreinhold/lean-rs/blob/main/crates/lean-rs-abi/src/supported.rs); the server and
`install-worker` refuse a numbered pin outside it and name the nearest supported release. Wider support starts in
lean-rs.

## Documentation

- [Tool catalog](docs/tool-catalog.md): each tool's request and reply.
- [Operations](docs/operations.md): run, upgrade and recover the server.
- [Architecture](docs/architecture.md): requirements, owners, and each invariant with its test.
- [Changelog](CHANGELOG.md): what changed in each release.
- [AGENTS.md](AGENTS.md): rules for work in this repository.

## License

MIT OR Apache-2.0. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
