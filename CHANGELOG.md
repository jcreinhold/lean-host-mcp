# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0/).

<!-- ASSISTANT: add new entries under [Unreleased]; `release-lean-host-mcp` promotes them on tag. -->

## [Unreleased]

### Added

- Initial release of `lean-host-mcp`, an MCP server that hosts Lean 4 in a supervised worker child
  (`lean-rs-worker-parent` + `lean-rs-worker-child`) and reaches the elaborator and kernel directly rather than through
  an external LSP.
- Two-crate workspace: a parent that does **not** link `libleanshared` and a per-toolchain worker binary that does —
  keeping the parent free of the Lean dylib so one server can host multiple toolchains.
- Multi-toolchain dispatch: each Lake project resolves its own `lean-toolchain` pin to a worker binary under
  `~/.local/share/lean-host-mcp/workers/<id>/`; `install-worker` subcommand builds and installs them.
- A six-tool declaration-centric proof workflow:
  `proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration`, plus
  `find_references` for semantic lookup. `proof_state` degrades to `{ "status": "unsupported" }` when the optional host
  shim is absent.
- Closure-channel actor over the worker child, with a `ProjectBroker` per-project pool and idle reaper.
- Stdio (default) and loopback-only Streamable HTTP transports.
- Response envelope contract (`result` + `freshness` + optional `warnings`/`next_actions`) shared by every tool;
  Lean-domain failures are part of the `Ok` payload, not MCP errors.
- Worker RSS supervision: a post-job restart policy and an in-flight hard-kill watchdog, plus the
  `rss_threshold_sweep.py` tuning tool.

### Notes

- Pre-1.0: minor versions may carry breaking changes; patch releases stay compatible.

[unreleased]: https://github.com/jcreinhold/lean-host-mcp/commits/main
