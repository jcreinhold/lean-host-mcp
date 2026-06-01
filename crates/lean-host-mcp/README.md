# lean-host-mcp

A Model Context Protocol (MCP) server that gives an AI agent direct, read-only access to a Lean 4 project's elaborator
and kernel. The agent can read the goal state at any point in a proof, get ranked lemma suggestions, test tactics, and
check whether a declaration type-checks — without editing a file.

Lean runs **in-process**, inside a supervised worker child, reached as in-process calls rather than messages to an
external LSP. A wedged tactic or runaway typeclass loop kills the child and the supervisor restarts it instead of taking
down the server. One running server can serve projects on different Lean toolchains at once.

## Install

```sh
cargo install lean-host-mcp                # the server
lean-host-mcp install-worker               # build a worker for each installed Lean toolchain
cd /path/to/your/lake/project && lake build
lean-host-mcp                              # serve over stdio from inside the project
```

`install-worker` compiles a per-toolchain worker (it links `libleanshared`, so it is built locally, not shipped as a
binary). It needs a Rust toolchain on `PATH` and the matching Lean toolchain installed via elan.

## Tools

`proof_state`, `search_for_proof`, `inspect_declaration`, `try_proof_step`, `verify_declaration`, and `find_references`.
Every call is non-mutating.

## Documentation

Full setup, the per-tool request/result schema, the response-envelope contract, tuning knobs, and the architecture
write-up live in the [repository](https://github.com/jcreinhold/lean-host-mcp): see `README.md`, `docs/tool-catalog.md`,
and `docs/operations.md`.

## License

MIT OR Apache-2.0.
