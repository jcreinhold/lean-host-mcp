# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0/).

<!-- ASSISTANT: add new entries under [Unreleased]; `release-lean-host-mcp` promotes them on tag. -->

## [0.3.0] - 2026-06-02

### Changed

- The default proof position is now the **pristine entry goal** — the state before any tactic runs. `proof_state` and
  `try_proof_step` previously disagreed: `proof_state`'s `goals_before` showed the entry goal, but `try_proof_step`
  spliced a candidate _after_ the first tactic, so a from-scratch tactic block read off `proof_state` failed with
  `introN failed: ... no additional binders to introduce`. Now `proof_state` at the default reports the entry goal
  (`goals_before == goals_after`) and a default `try_proof_step` snippet elaborates against that same goal, so
  from-scratch blocks work at the default. The old first-tactic state stays reachable as `{kind:"index","index":0}`.
  **Behavioral change** for callers that relied on the default mapping to the post-first-tactic state.
- Bumped the `lean-rs-worker-parent` / `-child` and `lean-toolchain` dependencies to 0.1.20, which adds the upstream
  `Entry` proof-position selector this reconciliation maps the default onto.

### Added

- For explicit `{kind:"index"}` / `after_text` positions, a failed candidate carrying a binder-introduction diagnostic
  now surfaces a cue pointing at the entry default (or continuing from `goals_after`), so the trap is signposted even
  off the default path.
- Worker provenance now records the building `lean-host-mcp` version. Worker and host are version-locked, so a worker
  built by a different host is flagged as stale — closing a skew that previously served an ABI/protocol-mismatched
  worker silently (it would fail at call time instead of with a clear message). `install-worker --list` gains a `host`
  column (`current` / `stale` / `unknown`), and a project served by a host-skewed worker now rides a rebuild warning in
  every envelope.
- `install-worker --auto` (the default) now rebuilds **stale** workers — host-version skew, `lean.h` header drift, or a
  failed/absent smoke record — not just missing ones, and skips out-of-window toolchains instead of failing on them. So
  re-running it after a `lean-host-mcp` upgrade brings every worker back in step. `--force` rebuilds current workers too.
- `install-worker --clean [--toolchain <id>]` removes all installed workers (or one); `install-worker --prune` removes
  only unservable workers (outside the supported window, or with a failed smoke test), keeping servable-but-stale ones.
  Both are idempotent and only touch the install root.
- Configurable per-request timeout: `runtime.request_timeout_millis` (env `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS`),
  default **120 s**. Replaces the worker's fixed 10-minute long-running profile, which let a whole-project scan (e.g.
  `find_references` at project scope) appear to hang. On expiry the worker is recycled and the call returns a retryable
  runtime error; raise it for unusually heavy modules, lower it to bound scans.

## [0.2.0] - 2026-06-02

### Changed

- Tools no longer advertise an `outputSchema`. Handlers return a bare `CallToolResult`, so `tools/list` carries no
  nested `$defs` (~52 KB → ~9.6 KB). The Anthropic Messages API dropped the field before the model anyway, and deep
  `$defs` broke strict clients (Claude Desktop, Zed) — proof agents read the JSON envelope as text either way.
  **Breaking** for any client that validated tool responses against the advertised schema.
- Per-call telemetry is now config-gated and omitted by default. `Freshness` splits into the always-emitted
  `FreshnessIdentity` (`project_root`, `session_id`, `lean_toolchain`) and an optional `Telemetry` block
  (`project_hash`, the full `imports` list, worker `RuntimeFacts`) that is dropped under the new default
  `telemetry.verbosity = quiet`; set `full` to restore today's output. `proof_state`'s `query_facts` and
  `search_for_proof`'s search funnel (stage counts, cache status) likewise appear only under `full`. The one actionable
  signal a worker restart carries still surfaces as a top-level `warning`.
- New `server.response_carrier` knob (`text` default, `structured`, `both`) selects whether the JSON envelope rides in
  `content` text, `structuredContent`, or both, instead of always duplicating into `structuredContent`.

### Removed

- The per-call tuning knobs `max_field_bytes`, `max_total_bytes`, and `heartbeat_limit` left the `inspect_declaration`,
  `try_proof_step`, and `verify_declaration` request schemas; they now live in `[output]` server config with the same
  defaults. **Breaking** for callers that set them per request — configure them server-side instead.

## [0.1.0] - 2026-06-01

### Added

- `cargo install lean-host-mcp` now works without a source checkout. When the server binary was not built from a
  checkout, `install-worker` builds each per-toolchain worker from the published `lean-host-mcp-worker` crate
  (`cargo install lean-host-mcp-worker --version =<ver>`) instead of erroring; from a checkout it still builds the
  worker from workspace source, and `--source-dir` overrides the choice. The worker is still compiled locally per
  toolchain (its rpath is machine-specific) and smoke-tested before use. Both crates are now published to crates.io.
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

[0.3.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jcreinhold/lean-host-mcp/releases/tag/v0.1.0
