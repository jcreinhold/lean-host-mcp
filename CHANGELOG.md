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
- Position tools (`goal_at_position`, `type_at_position`, `references_of_name`, `file_diagnostics`) backed by a
  content-hashed processed-file cache, degrading to `{ "status": "unsupported" }` when the host shim is absent.
- SQLite-indexed lookups (`find_symbol`, `find_lemma`, `outline`) and a Lean-free filesystem `project_scan` sweep.
- Closure-channel actor over the worker child, with a `ProjectBroker` per-project pool and idle reaper.
- Stdio (default) and loopback-only Streamable HTTP transports.
- Response envelope contract (`result` + `freshness` + optional `warnings`/`next_actions`) shared by every tool;
  Lean-domain failures are part of the `Ok` payload, not MCP errors.
- Worker RSS supervision with a post-job restart policy, plus the `rss_threshold_sweep.py` tuning tool.

### Notes

- Pre-1.0: minor versions may carry breaking changes; patch releases stay compatible.

[unreleased]: https://github.com/jcreinhold/lean-host-mcp/commits/main
