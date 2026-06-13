# lean-host-mcp

A Model Context Protocol server that gives an AI agent direct, read-only access to a Lean 4 project's elaborator and
kernel. The agent can read proof context, get ranked lemma suggestions, inspect declarations, test tactics, verify
declarations, and find semantic references — all without editing a single file.

It hosts Lean **in-process**: the elaborator and kernel run inside a supervised worker child and are reached as
in-process calls, not as messages to an external LSP. A wedged tactic or runaway typeclass loop kills the child, and the
supervisor restarts it instead of taking down the server. That is the difference from `lean-lsp-mcp`. One running
`lean-host-mcp` can serve projects on different Lean toolchains at once; it reads each project's `lean-toolchain` pin
and launches the matching pre-built worker.

## What it gives an agent

Five semantic tools, each with a `kind` mode:

- **`lean_context`** — proof context. Initial mode: `proof_position`.
- **`lean_trial`** — non-mutating experiments. Initial mode: `proof_step`.
- **`lean_verify`** — declaration verification. Initial mode: `explicit`.
- **`lean_lookup`** — declaration inspection, proof search, and references. Initial modes: `declaration`,
  `proof_search`, and `references`.
- **`lean_status`** — cheap project/toolchain/config status that does not open a worker.

Every call is non-mutating: the server reads source and elaborates in memory, and never touches your files. The typical
workflow and the request/result schema for each tool are in [`docs/tool-catalog.md`](docs/tool-catalog.md).

## Quick start

```sh
# 1. Install the server.
cargo install lean-host-mcp

# 2. Install a worker binary for each Lean toolchain you use. With no flag,
#    install-worker scans ~/.elan/toolchains and builds any that are missing or
#    stale; each lands under ~/.local/share/lean-host-mcp/workers/<id>/. The
#    worker is compiled locally per toolchain (it links libleanshared), so this
#    needs a Rust toolchain on PATH and the matching Lean toolchain via elan.
#    Worker and host are version-locked: re-run this after upgrading lean-host-mcp.
lean-host-mcp install-worker                       # build missing/stale workers
lean-host-mcp install-worker --toolchain v4.30.0   # or one toolchain
lean-host-mcp install-worker --list                # see what's installed (host column flags skew)
lean-host-mcp install-worker --clean               # remove all workers (e.g. to force a clean rebuild)

# 3. Run it from inside any built Lake project. The toolchain pin comes from
#    `lean-toolchain`, the project root from `lakefile.{lean,toml}`.
cd /path/to/your/lake/project
lake build && lean-host-mcp
```

Contributors working from a checkout install the same way but with `cargo install --path crates/lean-host-mcp` — from a
checkout, `install-worker` builds the worker from the workspace source (and `--source-dir` points it at a checkout
elsewhere). Build per-member, never `cargo build --workspace`, which would link libleanshared into the parent (see
[docs/operations.md](docs/operations.md)).

To pin a default project explicitly instead of relying on the working directory:

```sh
lean-host-mcp --lake-root /path/to/your/lake/project
```

## Connecting an MCP client

For a client that launches the server with a `command` (the common case, including Claude Code), stdio is the default:

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

You do **not** need to set `LEAN_SYSROOT` in the client config. The server picks the worker binary for each project from
its `lean-toolchain` pin and sets `LEAN_SYSROOT` invisibly per spawn, so one server process serves every toolchain you
have installed a worker for.

## Prerequisite: a built Lake project

A project the server can host needs only two things:

- A `lakefile.lean` or `lakefile.toml`.
- A successful `lake build`, so the `.olean` files for the modules the tools import exist on the search path. The plain
  `lake build` with no target is the usual step.

Dependencies need no extra configuration once their own `lake build` has run: the server reads `lake-manifest.json` and
adds each transitive package's build output to the import search path. For mathlib, `lake exe cache get` pulls
precompiled oleans; other dependencies follow the equivalent setup. Semantic proof search follows the same
zero-consumer-setup model through the package-owned `lean-semantic-search-runtime` crate, so consumer projects do not
declare or import it. `fixtures/lean/` is the demo target the test suite uses, and doubles as a minimal template to
copy.

## Transports

`lean-host-mcp` serves exactly one transport per process. Stdio is the default (see above). Streamable HTTP is selected
by `--bind` or `LEAN_HOST_MCP_BIND`:

```sh
lean-host-mcp serve --lake-root /path/to/your/lake/project --bind 127.0.0.1:8765
```

The default HTTP route is `/mcp`; override it with `--http-path /some-path` or `LEAN_HOST_MCP_HTTP_PATH`. `--http-path`
requires `--bind`; it never switches transports by itself. HTTP binds are loopback-only (`127.0.0.1` or `::1`): the
server has no built-in authentication or TLS, so non-loopback addresses are rejected rather than merely discouraged.

A client that accepts a URL connects to the HTTP route directly:

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

## Project resolution

Every tool call may pass its own `project="/abs/path"` to route that one call to a specific Lake root — useful when a
single client surveys several projects. A call that omits it resolves the project in this order:

1. `LEAN_HOST_MCP_PROJECT` (or the `--lake-root` flag)
2. Walk upward from the server's working directory for `lakefile.{toml,lean}`
3. `primary_project` in the config file (`./lean-host-mcp.toml` or `~/.config/lean-host-mcp/config.toml`)

All tunable knobs (worker memory ceilings, pool sizing, transport) can also be set in that config file instead of env
vars. Run `lean-host-mcp config init` to write a documented starter with every option at its default, then edit it. See
[Configuration file](docs/operations.md#configuration-file) for discovery and precedence, and
[Configuration reference](docs/operations.md#configuration-reference) for the full per-knob table.
`broker.semantic_permits` is a per-user cross-process admission limit by default; parallel servers that share the same
semantic lock directory queue worker-opening semantic calls until a permit is free. Metadata-only degraded responses and
project-scope `.ilean` reference reads do not open workers and do not consume permits.

## Response Shape

Every public tool returns the same semantic outer shape. `data` is mode-specific; `errors` is a structured issue channel
for runtime failures and warnings; `trust` is the small project/session identity plus optional artifact facts.

```jsonc
{
  "data": { /* mode-specific */ },
  "errors": [],
  "trust": {
    "project_root": "/abs/path",
    "session_id": "uuid-or-metadata-only",
    "lean_toolchain": "leanprover/lean4:v4.31.0-rc2",
    "artifacts": [
      {
        "artifact": "source",
        "scope": "file",
        "status": "edit_fresh",
        "path": "My/Module.lean"
      }
    ]
  }
}
```

Artifact facts use stable tokens: `artifact` is `source`, `olean`, `ilean`, or `worker`; `scope` is `file`, `module`,
`project`, or `toolchain`; `status` is `edit_fresh`, `build_fresh`, `stale_build`, `missing_build`, `unknown`, or
`not_applicable`. The `artifacts` array is omitted when no tool has a proof-relevant artifact fact to report. Quiet
telemetry never removes these trust facts.

The split that matters: **Lean-domain failures** (parse errors, elaboration diagnostics, kernel rejection, meta timeout)
ride inside `data` — a failed proof is still a successful call. **Recoverable runtime failures** (admission or mailbox
pressure, worker death, session loss) appear in `errors` with a retryable flag and structured details. **MCP errors** are
reserved for I/O/config failures and unusable Lake projects. By default the semantic response rides as JSON text in
`content`; `server.response_carrier` (`structured` / `both`) can place it in `structuredContent` instead. Tools
advertise no `outputSchema` — the Anthropic Messages API drops it, and deep `$defs` break strict clients.

## Documentation

- [`docs/tool-catalog.md`](docs/tool-catalog.md) — the semantic tool workflow and the per-mode request/result schema.
- [`docs/operations.md`](docs/operations.md) — tuning knobs, transport internals, the runtime-error contract, and the
  test/perf harness.
- [`docs/architecture.md`](docs/architecture.md) — how the server is built (for contributors).

## Versions

`lean-host-mcp` 0.4.1 builds on `lean-rs-worker-parent` / `-child` 0.2.2, which transitively pin `lean-rs` /
`lean-rs-host` 0.2.2. It supports the Lean window `4.26.0 ..= 4.31.0-rc2` and is built and tested against the head of
that window, Lean **4.31.0-rc2**.

A project brings its own toolchain: the server hosts whatever Lean version the project's `lean-toolchain` pins, as long
as it falls inside the supported window. The window is read directly from `lean-toolchain::SUPPORTED_TOOLCHAINS` (itself
sourced from [`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain)), never
duplicated here. A pin outside the window is rejected when the project opens, with a one-line verdict naming the window
and the nearest supported version, and `install-worker` refuses to build for it.

Widening the window is a `lean-rs` change first, then a version bump here.

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
