# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0/).

<!-- ASSISTANT: add new entries under [Unreleased]; `release-lean-host-mcp` promotes them on tag. -->

## [Unreleased]

### Added

- Unified TOML config file for every tunable knob. A `lean-host-mcp.toml` (found by walking up from the working
  directory, like the lakefile) or the home `~/.config/lean-host-mcp/config.toml` can set the `[runtime]`, `[broker]`,
  and `[server]` knobs that were previously env-var-only, plus the existing `primary_project`. When both files exist
  they merge per key (local wins); precedence is `CLI > env var > file > built-in default`, so existing
  `LEAN_HOST_MCP_*` setups are unaffected and an env var still overrides the file. Malformed files are logged and
  ignored. See [docs/operations.md](docs/operations.md#configuration-file).
- `lean-host-mcp config init` writes a documented starter config file — every option at its current default, each with a
  comment explaining it — to `./lean-host-mcp.toml` (or `~/.config/lean-host-mcp/config.toml` with `--home`, or a
  `--path`). The file, the per-knob reference table in the docs, and the built-in defaults are all generated from one
  in-code catalogue, so they cannot drift.
- Worker-recycle observability: each recycle is now logged to stderr with structured fields (`cause`, `reason`,
  `worker_generation`, `rss_kib`, `limit_kib`, `restarts_total`) at a signal-appropriate level (`warn` for
  abnormal/crash, `info` for memory-pressure cycles, `debug` for hygiene), and every response's `runtime` carries
  lifetime `restarts_total` plus a per-cause `restarts_by_cause` breakdown so recycle *frequency* is visible. See
  [docs/operations.md](docs/operations.md#observing-worker-recycles).
- Structured `tracing` across the server's high-value paths (tool entry, project open/eviction, the idle reaper, the
  per-call job span, RSS headroom, toolchain resolution, and verdict-relabel decisions), all on stderr so the stdio
  transport's stdout stays clean. Default level is `info`; `RUST_LOG=lean_host_mcp=debug` surfaces per-call detail.
- RSS-config guard rails: the server validates `import_switch <= post_job <= hard_kill` at startup and refuses to start
  with a clear `invalid RSS config: …` message on an inverted ordering, so e.g. raising
  `LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB` above the hard-kill ceiling fails fast instead of degrading silently.
- Honest `worker_recycled` verdict: when the worker is recycled or restarted *during* a semantic call (a memory-pressure
  recycle on a heavy module, or a crash-and-retry), the verdict was computed under infrastructure duress.
  `verify_declaration` now relabels a non-positive verdict to `verification_status: "worker_recycled"` with
  `facts_trustworthy: false` instead of a misleading `not_found`, and `try_proof_step` / `proof_state` carry a retry
  warning. A `verified` verdict is never relabeled (verification is monotone). The signal is derived from the call's
  runtime facts (`call_restart`) and excludes benign pre-job/planned cycles.

## [0.1.0] - 2026-05-31

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
- Honest resolution verdicts: an incomplete project build degrades to a single `needs_build` verdict carrying a
  `lake build` cue across `verify_declaration`, `inspect_declaration`, `proof_state`, `try_proof_step`,
  `find_references`, and `search_for_proof` — never a misleading `ambiguous` status or a hard transport error. Genuine
  ambiguity instead names the competing declarations, and `facts_trustworthy` flags any verdict computed against an
  incomplete or unresolved environment.
- Builds on `lean-rs-worker-parent` / `-child` 0.1.19 (worker protocol 8), supporting the Lean toolchain window
  `4.26.0 ..= 4.31.0-rc1` (head `4.31.0-rc1`).

### Notes

- Pre-1.0: minor versions may carry breaking changes; patch releases stay compatible.

[unreleased]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jcreinhold/lean-host-mcp/releases/tag/v0.1.0
