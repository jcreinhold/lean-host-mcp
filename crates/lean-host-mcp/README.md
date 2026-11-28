# lean-host-mcp

A Model Context Protocol server that runs one Lean 4 project's elaborator and kernel for AI agents: proof state,
declaration lookup, tactic trials, verification and Lake builds, shared by many sessions.

```sh
cargo install --locked lean-host-mcp
lean-host-mcp install --lake-root /path/to/project
```

Read the [README](https://github.com/jcreinhold/lean-host-mcp/blob/main/README.md) before you serve a project: it covers
safety, client setup and the supported versions, and links the rest of the documentation.

## License

MIT OR Apache-2.0.
