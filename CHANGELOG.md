# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0/).

<!-- ASSISTANT: add new entries under [Unreleased]; `release-lean-host-mcp` promotes them on tag. -->

## [Unreleased]

### Added

- `lean-host-mcp install --lake-root PROJECT` installs everything one project needs under
  `PROJECT/.local/lean-host-mcp/`: a copy of the server in `bin/`, which is also the SDK writer that builds the worker's
  shims; the worker for the project's `lean-toolchain` in `workers/`; a private `store/`; and a local `config.toml` that
  records their absolute paths under `[install]`. `--dir` moves `bin/`, `workers/` and `store/`. It never writes a
  tracked file.
- One server per store: a server with `--lake-root` locks `store/server.lock`, and a second one exits and names the pid
  and Lake root of the first. `LEAN_HOST_MCP_STORE_DIR` and `[install] store_dir` move the store.
- `lean_build` runs Lake targets, or a recipe of Lake steps from the config, under the project's write turn. Checks wait
  for it. The reply gives each step's command and exit code, the path of the log in `store/logs/`, and the log's tail.
- `lean_imports` lists each file's whole import closure from `lake --no-build setup-file`, and the source of each module
  the root built as `lake query +MODULE:lean` names it under the library's `srcDir`. A missing or stale import fails the
  call with `setup_file_failed` and Lake's own message.
- Config tables `[build]` (`guard`, `timeout_secs`, `log_limit_mib`) and `[recipes.<name>]` (`steps`, `append`,
  `guard`). `config check --lake-root PROJECT` checks both config files and prints these tables without running Lake.
- `[install] elan_home`: install records the elan home, `ELAN_HOME` or else `$HOME/.elan`. When `ELAN_HOME` is unset,
  the server and `install-worker --lake-root` find the Lean toolchain there. Lake, Cargo and helper children get
  `ELAN_HOME` set.
- `doctor processes --lake-root PROJECT` lists that project's servers from the records in its store.
- Named requests. A tool called with `request_id` gets a row in `store/requests.sqlite3` before it starts, runs at most
  once and keeps its result: the same id and arguments return that result, from any client and after a restart, and
  other arguments fail with `request_id_conflict`. A dropped connection ends only the client's wait. `lean_run` starts a
  named request and replies at once; `lean_get` returns its row and, once it has ended, its result. A build's row says
  `running` only once it holds the write turn and the writer lock, and a check's once it holds its read turn. At start
  the server ends the rows an earlier server left: `running` rows become `interrupted` and never run again, and `queued`
  rows run. A call without an id gets a `srv-` id in its reply. `rusqlite`, with SQLite built in, is now a required
  dependency.
- Config table `[requests]` (`retention_hours`, `max_rows`, `wait_max_secs`) and the key `recipes.<name>.timeout_secs`.
  `lean_runtime` reports `requests: {queued, running}` and the capability `durable_requests_v1`.

### Changed

- The server reads config only at its `--lake-root`: the tracked `lean-host-mcp.toml`, then the ignored
  `.local/lean-host-mcp/config.toml`, whose keys win. Precedence is CLI > env > local file > tracked file > default.
- The server and `install` share one worker directory: `LEAN_HOST_MCP_WORKERS_DIR`, then `[install] workers_dir`, then
  `.local/lean-host-mcp/workers` under the Lake root. `install-worker` needs `--lake-root`, `--workers-dir` or that env
  var.
- A missing or unusable worker gives one line naming the toolchain, the worker directory and the fix, `SERVER install
  --lake-root PROJECT`. A protocol mismatch carries the same recovery command.
- `config init` writes `lean-host-mcp.toml` in the working directory or at `--lake-root`.
- Checks and builds share one fair lock per project. A check waits in arrival order behind a queued build instead of
  failing busy, and a build waits for running checks. A build outside the server that holds the project's writer lock
  makes new checks wait the same way. A raw `lake build`, which takes no lock, is outside this contract.
- Setup-file runs as a native reader with `--no-build`, under the check's shared lock, and never as a writer.
- Every reply whose result came from a file's text names that file with the SHA-256 of the bytes it checked, in a
  `source` trust row's `content_sha256`. `needs_build` replies from `lean_context`, `lean_lookup` `declarations` and
  `references`, and `proof_search` from a file, now carry it too. A reply that checked one file also has it as
  `trust.source_sha256`.
- A worker reloads its imports after a build only as a backstop: the finished-build count replaces the import stamps. A
  cached query result lives only as long as the worker that computed it.
- A server serves one Lake project, its `--lake-root` or `LEAN_HOST_MCP_PROJECT`, and fails every call that names
  another root, metadata calls too. Without a root every call fails; the server no longer looks for a lakefile above its
  working directory. `BrokerConfig::lake_root` replaces `env_default` and `cwd`, `max_workers` replaces `max_projects`,
  `BrokerConfig::serving(root)` builds the default config for one root, and `BrokerConfig::from_env_with_file` replaces
  `pool_from_env`, `pool_from_env_with_file` and `worker_limits_from_file`.
- The store keeps the state that went under `HOME`: the SDK's native owner records in `owners/`, the semantic runtime
  cache in `semantic-runtimes/` and the server process records in `processes/`. Serve and install bind the owner
  directory before any native work. `LEAN_RS_WORKER_OWNERS_DIRECTORY`, `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` and
  `LEAN_HOST_MCP_PROCESS_REGISTRY_DIR` still move each one. Install, then serve, write nothing under `HOME` but the
  socket directory.
- Breaking: build against lean-rs 0.11.0 and lean-semantic-search 0.8.0. The server, its writer and its workers must
  come from one SDK release. A worker built against lean-rs 0.10 still reports protocol 13; a plain `install` replaces
  it, since another release built it. A lock record or owner-directory entry that a 0.10 storage turn left now refuses
  writer admission; [operations](docs/operations.md#upgrading-from-lean-rs-010) gives the steps to clear them.
- Breaking: the server binary serves the SDK's own writer, reader and guardian helpers (`lean_rs_writer::HelperCommand`)
  in place of the host's copy, so their subcommands, frames and messages are the SDK's. A helper started without
  `LEAN_RS_WORKER_OWNERS_DIRECTORY` refuses to run; the SDK sets it on every helper it starts. The capped build log
  takes `LEAN_RS_WRITER_LOG` and `LEAN_RS_WRITER_LOG_LIMIT` in place of `LEAN_HOST_MCP_LOG` and
  `LEAN_HOST_MCP_LOG_LIMIT`, and a cut log ends with `[lean-rs-writer: log limit reached; later output dropped]`. The
  writer reads only its `LEAN_RS_*` settings, so `LEAN_HOST_MCP_WRITER_LOCK_LOANS` and the other
  `LEAN_HOST_MCP_WRITER_*` names are gone.
- Breaking: `install` and `install-worker` lose `--shim-source`. The new worker writes the shim sources compiled into it
  to its generation's `shims/` directory and builds them there under the writer, so the installer no longer reads the
  Cargo source cache or checks the shim files against a pinned list.
- The semantic runtime comes from lean-semantic-search 0.8, which builds its Lean modules with `lake build` under the
  server's writer and no longer links a shared library.
- `install` and `install-worker --auto` count a worker that another `lean-host-mcp` release built as stale and rebuild
  it, even when its protocol matches: the worker and host crates release together, and a release can keep the protocol.
  The server still serves such a worker.

### Removed

- Remove the managed mode: the `lean_managed_admission` tool, the `managed_original` tool argument, the launcher and
  writer setup subcommands, the managed serve flags, and the `managed-native-store` and `managed-warm-inspections`
  features. The server always runs its ordinary mode.
- Remove config lookup in the home directory, the working directory and its parents, with `LEAN_HOST_MCP_CONFIG_DIR`,
  `config init --home` and the `primary_project` key. Neither config lookup nor the worker directory depends on `HOME`.
  The server ignores an old `[launcher]` table.
- Remove the serve flags `--module-setup-writer`, `--module-setup-storage-request`, `--prepared-generation` and
  `--verification-approval`, with prepared worker generations and the writer's setup-file path.
- Remove the owner directory's fallback to the platform's local data directory and the semantic cache's fallback to the
  user cache directory. Without a store, native work needs `LEAN_RS_WORKER_OWNERS_DIRECTORY`, and semantic tools need
  `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`.
- Remove `doctor writer-rollback`.
- Remove MCP Tasks; the `lean_request` tool with its tickets, cancel and acknowledge; the request journal with
  `--request-journal` and `LEAN_HOST_MCP_REQUEST_JOURNAL`; `request_registry` in `lean_runtime`; and the capabilities
  `retained_request_lifecycle_v3` and `retained_request_binding_v1`. Named requests replace them and have no cancel.
- Remove `ProjectRuntimeConfig::with_operator_primary`: `BrokerConfig::lake_root` alone names the served root. Tests and
  benches that built a broker with `ProjectBroker::new` set the root but never approved it, so their native calls
  failed. The `multi_project` tests and the `multi_project_dispatch` bench, which need several projects in one server,
  are gone.
- Remove `LEAN_HOST_MCP_MAX_PROJECTS` and `[broker] max_projects`, the old names of `max_workers`; `config check` now
  names `max_projects` as an unknown key. Remove the `max_projects` field from `lean_status`'s `broker` and from
  `lean_runtime`'s `effective_settings.broker` and `pool`.
- Remove the shim build when a worker opens. The server reads the shims `install` built and fails with the install
  command when they are missing or stale; the drain that build took, and the stage `host_artifact_preparation`, are
  gone.
- Remove the server's own lakefile parse, with `LakeProjectMeta`'s `package`, `library` and `umbrella_module` fields,
  `LakeProjectMeta::discover_from`, and the `regex` dependency. Nothing read those fields; Lake evaluates the
  configuration itself. A root still needs a `lakefile.toml` or `lakefile.lean`.
- The library exports less. `config_file`, `config_schema`, `envelope`, `error`, `lake_meta`, `projections`,
  `runtime_compatibility`, `server`, `status` and `trust` are private, and the crate root re-exports only the types the
  binary, tests and benches name. The projections `project_elab_result`, `project_kernel_result`,
  `project_meta_rendered`, `project_meta_bool` and `project_declaration_row`, which nothing called, are gone with their
  types `ElabSuccess`, `KernelOutcome`, `KernelSummary`, `MetaOutcome` and `DeclarationRow`.
- Remove the `lean_runtime` lease. The tool takes no arguments and only reports. Gone are the actions `acquire`,
  `drain`, `renew` and `release` with `lease_id`, `lease_seconds` and `retire_idle`; the reply's `status`, `detail`,
  `lease_id` and `retirement_requested`; `pool.admission_paused` and `pool.lease_remaining_millis`; the reasons
  `worker_pool_leased` and `worker_pool_draining`; the capability `draining_lease_v1`; and the library's
  `RuntimeAction`, `RuntimeControlRequest`, `RuntimeControlResult` and `RuntimeControlStatus`. With many sessions on one
  server, one session's lease paused every other session's calls.
- Breaking: remove `LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER` and the `source_handoff` field of `lean_compatibility`'s
  `shared_writer`. lean-rs 0.11 removed the source handoff and the storage fence that field reported.
- Breaking: remove the library's `shared_writer` module, the host's copy of the SDK writer. Use
  `lean_rs_writer::use_helper_executable`, `lean_rs_writer::contract` and `lean_rs_writer::WriterContract`;
  `store::owner_directory` replaces `ownership_registry`. Its lifecycle and contract tests went with it, since
  `lean-rs-writer` tests the same code, and so did `tests/writer_cli_domain.rs`, whose CLI clients are now the SDK's.
- Remove the `build_development_shims` example; the worker's preparation mode builds the shims it carries.
- Breaking: remove `install-worker --standalone-writer` and the standalone SDK writer. The installers build a worker's
  shims with the server binary, which serves the SDK's writer: `install` with its copy in `bin/`, the file the server
  runs as, and `install-worker` with its own executable. `install-worker` no longer looks for `lean-rs-writer` beside
  itself, and `install` no longer installs `bin/lean-rs-writer` or runs `cargo install`. An old `bin/lean-rs-writer` can
  be deleted.

### Fixed

- A file check whose import is out of date returns a `needs_build` reply, with a `stale_build` row for each module Lake
  lists, in place of `internal_error`. `lake --no-build setup-file` exits 3 when it would have to rebuild an import, and
  the server now reads that code.
- `serve` checks the worker socket directory when it starts and exits if the directory is unsafe or a worker socket path
  in it would be too long. Before, the first file check found out.
- File checks that start together no longer fail with `another transient reader admission gate is active`: a check that
  finds the SDK's gate on the installed shims held tries again every 20 ms until its request's time runs out.
- A native reader whose start finds an ended reader's input file gone tries again, up to three times, instead of failing
  with `native utility refused before bootstrap spawn: No such file or directory`.
- `lean_status` `file_diagnostics` reports what `lake build` reports. It runs `lean --json` on the file from the project
  root with the setup from `lake --no-build setup-file`, in place of the worker's file driver. That driver checked the
  body without its header, so a header linter such as Mathlib's warned on every file, and it reported messages Lean
  marks silent, such as those of a compiler plugin. Lean reads the hashed text on stdin under the file's name, so an
  edit during the check does not change what it checked.
- The server creates a missing `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`. A relative path, or one it cannot create, fails with
  an error that names the variable and the path, in place of a bare `io: No such file or directory`.
- A worker that exits before it connects no longer leaves its owner record in phase `dispatching`, where it held back
  every later build that used the owner directory until someone deleted it. lean-rs 0.11 closes the record once it reaps
  the child.
- `lean_verify` and the declaration outline of a file check the whole file, header included, and drop the messages Lean
  marks silent, as `lake build` does. lean-rs 0.10's file driver checked the body without its header, so a header linter
  such as Mathlib's warned on every file. File-backed proof search still elaborates the body apart from its header.

## [0.16.22] - 2026-11-25

### Fixed

- Preserve managed proof verdicts and axiom results when informational diagnostics exceed the display budget.

## [0.16.21] - 2026-11-24

### Fixed

- Group independent verifier modules under one admitted writer and retain bounded readiness failures and artifact trace
  results.
- Bind SDK helpers to the existing ownership registry and enforce the original deadline before cold inspection dispatch.
- Settle acknowledged warm attempts after exact generation retirement so a normal drain permits rollback without replay.

## [0.16.20] - 2026-11-24

### Fixed

- Use one broker capacity limit for managed readiness, trace snapshots, utility readers, and warm workers. Keep slots
  charged until the exact execution scope closes.
- Transfer managed readiness locks to the verified writer helper. If the helper fails before dispatch, return capacity
  only after exact exit and reacquisition of every original lock.
- Allow launcher rollback after confirmed custody closure when the original result is unknown. Preserve the original key
  and refuse replay.

## [0.16.19] - 2026-11-24

### Fixed

- Let managed HTTP callers share the project selector lock while one broker owns the private store directory.
- Promote enrolled warm stores only during an exclusive launcher transition, so a running cold broker cannot share a
  store during promotion.

## [0.16.18] - 2026-11-23

### Added

- Opt-in warm managed declaration lookup reuses a verified worker for the same prepared imports. Guarded writes retire
  the worker, and original results remain recoverable by key.
- Managed preparation derives the selected import closure, checks pinned cache inputs, and builds first-party modules in
  dependency order.
- Managed file-backed proof context, command and tactic trials, proof search, and declaration lookup use admitted
  inputs. Managed root-library builds, blueprint declaration checks, and read-only regrouping retain bounded results.

### Fixed

- Initialize Lake project discovery patterns before serving requests to avoid a stack overflow during ordinary proof
  search.
- Verify installed packages with the matched lean-rs 0.10.15 worker and writer. Run launcher fault injection on a
  separate executable.

## [0.16.17] - 2026-11-23

### Added

- Opt-in managed project selection with guarded rollback, original-key result recovery, and verified custody closure.
- Managed root architecture lint and fixed prebuilt Lean tooling suites, with bounded diagnostics and retained preflight
  refusals.

### Fixed

- Setup-aware file verification records the executing CLI toolchain and exact inputs separately from worker
  compatibility.
- Managed first-party clean checks the effective Lake output layout before deleting build files.

## [0.16.16] - 2026-11-22

### Fixed

- The embedded writer now admits work after a caller-owned SDK writer turn has closed and settled its storage charge.

## [0.16.15] - 2026-11-22

### Fixed

- File diagnostics prioritize Lean errors when earlier informational messages exhaust the output budget.

## [0.16.14] - 2026-11-21

### Added

- Native readers can select `native_reader_stdout_frame_v1` to retain a complete result when stderr clips. Oversized
  required stdout reports a stopped operation after confirmed closure.

## [0.16.13] - 2026-11-21

### Fixed

- File-wide verification and file diagnostics now use the current source and Lake file setup for ordinary projects.
  Diagnostics preserve Lean messages, source ranges, and truncation.

## [0.16.12] - 2026-11-21

### Fixed

- Installer smoke, worker sessions, and file-backed verification and search use a private socket directory. Set
  `LEAN_HOST_MCP_SOCKET_DIR` to select it explicitly. Durable worker records stay in their existing registry.
- The embedded writer checks its complete lock set for outstanding storage charges before starting work. An overlapping
  charge refuses admission while a disjoint project can proceed.

## [0.16.11] - 2026-11-21

### Fixed

- Compact file verification uses the lean-rs 0.10.3 compact verdict and reports whether axiom data is available or
  truncated.
- Registry worker installation stages Cargo output separately from the selected worker generation.

## [0.16.10] - 2026-11-21

### Fixed

- Ship the bounded Lake module setup transfer described below. Version 0.16.9 did not publish because the release
  workflow selected an incomplete Lean prefix. Release verification now checks the header and shared library under the
  prefix reported by the selected Lean executable.

## [0.16.9] - 2026-11-21

### Fixed

- File-backed verification and search now accept Lake module setup data up to 16 MiB from the selected no-build setup
  command. Large import maps no longer exhaust the generic writer output limit.

## [0.16.8] - 2026-11-21

### Changed

- Adopt published lean-rs 0.10.2 and use Lean 4.36.0 for the release fixture.
- Verify current project files in a supervised Lean process using the selected Lake module setup. Check project inputs
  again when transferring them from writer to reader custody. A missing or changed input prevents verification.
- Use semantic runtime 0.7.9 to elaborate file-backed search goals with the selected Lake module setup.
- Bind native workers, build helpers and external readers to one supervised process scope and canonical artifact-lock
  union. Preserve confirmed stop evidence through timeout and replacement failures; recover capacity after actual
  cleanup without replaying Lean commands.
- Replace rotating retained-request admission epochs with durable prepared tickets and bounded backpressure. Unresolved
  request identities remain protected across restart and lost replies.
- Enforce compiled host ABI expectations before every first or replacement session. Add runtime-free
  `lean_compatibility` discovery with explicit `pure_v1` mode.
- Reevaluate Lake options under shared custody with generation-owned private caches, preserving warm workers and saved
  root options. Require matched SDK 0.10.2 and worker protocol 13.

## [0.16.7] - 2026-11-14

### Fixed

- Separate HTTP drain tests from cold worker startup; test real-worker resumption after warm-up.
- Resolve private declarations without duplicate verification targets, including UTF-16 source positions.
- Use lean-rs 0.9.9 to report private theorem axioms correctly and reuse source headers without repeated imports. Retire
  workers cleanly when import accounting is unavailable instead of treating planned retirement as a crash.
- Report successful worker retries as completed checks while retaining the warning about the earlier interruption.
- Bound project queue waits, reject expired dispatches, and let queued builds drain admitted requests fairly.
- Keep completed results for a bounded period, recover journal writes after storage failures, and isolate damaged
  records.

### Changed

- Worker protocol 12 requires a matching parent and worker. Update both together, then restart the server. Lean's
  supported toolchain range is unchanged.

## [0.16.5] - 2026-11-14

### Fixed

- Source diagnostics, proof trials and verification now use the target module's Lake compilation options, including
  `autoImplicit`. Changing options invalidates cached answers. Responses show the project options and host heartbeat
  limit. This fixes checks that previously accepted code rejected by a project's normal build.
- Direct dependency configuration keeps `require ... with` settings and compiles into temporary storage, without
  overwriting an active build's configuration. Unsupported transitive dependency source checks and extra compiler
  arguments fail clearly rather than fall back to defaults. No imports or Mathlib modules are built during discovery.
- Update lean-rs to 0.9.8. Worker protocol 11 rejects older workers rather than silently ignoring compilation options.
  Update the parent and worker together, then restart any running server.

### Changed

- Source requests resolve Lake configuration before checking the result cache. This currently adds about 0.6 seconds per
  root-project request; direct dependency checks cost more. Lookups of built declarations are unchanged.

## [0.16.4] - 2026-11-14

### Fixed

- Update lean-rs to 0.9.7, which reuses unchanged module headers after source edits instead of repeatedly importing
  them. Header, session, options and file-context changes invalidate reuse; failed header imports retry normally. Worker
  resource limits and crash handling remain unchanged.

## [0.16.3] - 2026-11-13

### Fixed

- Interrupted worker calls no longer enter the module query cache. Retrying after a crash, timeout, or cancellation runs
  the check again instead of returning a cached empty result without the original warning.
- File diagnostics now report `runtime_unavailable` with no result when the worker fails to finish the call reliably,
  rather than presenting an empty diagnostic list as a successful check. Completed checks still cache normally after
  planned worker recycling.

## [0.16.2] - 2026-11-13

### Fixed

- Supported import budgets below the static sizing estimate no longer produce a startup warning. The estimate is now
  DEBUG-only; warnings for conflicting heap limits and runtime failures remain unchanged.

## [0.16.1] - 2026-11-13

Version 0.16.0 stopped in CI before publication. This release includes its changes and the Rust 1.100 lint fix.

### Added

- Optional durable request journal with epoch-bound request IDs, explicit acknowledgment, and execution status separate
  from proof results. Restarted servers retain saved results without replaying work; interrupted execution remains
  explicitly unknown.

### Fixed

- Acknowledged-result compaction preserves the request that triggered it.
- Journal ownership releases explicitly on drop, even while a spawned child briefly retains an inherited file
  descriptor.
- Parallel test log capture no longer depends on process-wide tracing callsite state.

## [0.15.0] - 2026-11-11

### Added

- Human-readable INFO request/response summaries with generated correlation numbers, tool/kind, bounded project labels,
  elapsed time, allowlisted outcomes and result counts. Proof content, diagnostics and caller-supplied IDs are excluded;
  retained polling stays quiet. Default logging suppresses routine SDK session churn but preserves warnings, errors and
  explicit verbose overrides. Configuration advisories are emitted once per broker startup.
- Retained `lean_request` submission, retrieval and cancellation for clients without MCP Tasks negotiation.
  Client-selected request IDs are registered before execution, deduplicated against exact inputs, and bound to the
  server instance, tool, request digest and project. Terminal results remain available for ten minutes while the server
  process lives; they are not durable across restarts.
- MCP Tasks support for reconnectable work and cancellation that remains responsive during silent worker execution.
  Existing token-gated progress reporting remains available across the semantic tool families.
- `lean_runtime` status and expiring idle-pool leases for coordinating external work. Status reports the build revision,
  effective settings, worker activity and retained-request counts. Idle retirement waits for confirmed worker shutdown
  before capacity is reusable.

### Changed

- Busy-project mailboxes and cold-project capacity admission wait in FIFO order within bounded deadlines instead of
  making clients race for capacity. Same-project opening waits share the admission deadline.
- Updated compatible dependencies to rmcp 3.9, Tokio 1.54.1 and published lean-rs 0.9.6, including the fixes for
  RUSTSEC-2026-0386 and RUSTSEC-2026-0461. The Lean toolchain window remains 4.30.0 through 4.35.0.
- Native-only HTTP rejects every present browser `Origin` with HTTP 403 while retaining loopback binding and Host
  validation. Modern MCP 2026-07-28 stateless requests and legacy sessions are covered.
- Invocation and runtime failures set MCP `isError` while preserving the semantic envelope. Negative Lean proof verdicts
  remain domain results. Malformed tool arguments have the same error shape with and without Tasks negotiation.

### Fixed

- Linux process diagnostics tolerate zero or unavailable process-group IDs without panicking. Restricted observers use
  procfs only as positive presence evidence; a failed observation still never authorizes stale-record cleanup.
- Worker startup failures and uncertain replacement shutdowns no longer release capacity before child exit is confirmed.
  Cancellation preserves the original request deadline and observes a worker that emits no progress.
- Unavailable process observations and relative executable labels remain inconclusive rather than being treated as
  evidence of process death.
- Deprecated Rust lint names and SDK aliases, explicit lint-group priorities, and release installation guidance using
  `--locked`.

### Security

- Clarified the trusted-local-client boundary: Lean commands, tactics and import initializers may perform IO with server
  privileges. The worker is process isolation, not an OS sandbox.

## [0.14.2] - 2026-10-13

### Changed

- **Widened the toolchain window head to Lean 4.35.0.** The requirements rise to lean-rs 0.9.4 and lean-semantic-search
  0.7.7 on the unchanged 0.9 / 0.7 lines. The final 4.35.0 ships a `lean.h` byte-identical to 4.35.0-rc3 and 4.35.0-rc4,
  so both join the existing window entry and no bindings change. The supported window is now `4.30.0 ..= 4.35.0`. The
  fixture pin, the worker the server is built and tested against, and the head-toolchain literals in the test suite and
  docs all move to v4.35.0. The Rust floor is unchanged at 1.94.

## [0.14.1] - 2026-09-25

### Changed

- **Widened the toolchain window head to Lean 4.35.0-rc3 and added 4.34.1 to the window.** The requirements rise to
  lean-rs 0.9.2 and lean-semantic-search 0.7.6 on the unchanged 0.9 / 0.7 lines, because `lean-toolchain` 0.9.1's build
  script rejects the 4.34.1 and 4.35.0-rc3 `lean.h`. Both ship new digests carrying a refcount-helper patch with no
  struct declaration changes. The supported window is now `4.30.0 ..= 4.35.0-rc3`. The fixture pin, the worker the
  server is built and tested against, and the head-toolchain literals in the test suite and docs all move to
  v4.35.0-rc3. The Rust floor is unchanged at 1.94.

### Fixed

- **API docs no longer link to private items.** Thirteen rustdoc warnings (links to crate-private items and one link to
  a method that does not exist) rendered as dead text on docs.rs. The public `ProjectRuntimeConfig` accessor docs now
  state the memory and pool policy directly instead of pointing at private constants, and the envelope module doc now
  describes the `quiet` telemetry gate accurately. CI and `scripts/prerelease.sh` now build both crates' docs with
  `-D warnings`.

## [0.14.0] - 2026-09-25

### Changed

- **Widened the toolchain window head to Lean 4.35.0-rc2 and added 4.34.0 and 4.35.0-rc1 to the window.** The `lean-rs`
  / `lean-semantic-search` line is unchanged at 0.9 / 0.7, but the requirements rise to lean-rs 0.9.1 and
  lean-semantic-search 0.7.5, because `lean-toolchain` 0.9.0's build script rejects the 4.35 `lean.h`. lean-rs 0.9.1's
  `SUPPORTED_TOOLCHAINS` adds 4.34.0 (byte-identical header to 4.34.0-rc2), 4.35.0-rc1, and 4.35.0-rc2 (new digests, no
  struct declaration changes). It also fixes scalar-array element sizes for arrays that 4.35 marks as linear. The
  supported window is now `4.30.0 ..= 4.35.0-rc2`. The fixture pin, the worker the server is built and tested against,
  and the head-toolchain literals in the test suite and docs all move to v4.35.0-rc2. The Rust floor is unchanged at
  1.94.

## [0.13.0] - 2026-09-02

### Added

- Every tool emits token-gated progress notifications (start, 15-second heartbeats, completion), not only `lean_verify`,
  so clients that extend their deadline on progress no longer abandon a cold import or a long elaboration in
  `lean_trial`, `lean_context`, `lean_lookup`, or `lean_status`.
- Request synonyms: recurring mode and field spellings (`signature`, `tactic`, `command`, a bare `file` for
  `lean_lookup(kind="declarations")`, a top-level or singular `lean_verify` target, string target groups, and more) are
  normalized onto the canonical schema before decoding. A mode that names another tool's job, such as `search`, is
  rejected with an error naming the tool and mode to call instead.
- `lean_verify` `not_found` rows carry the file's declarations with a similar short name in `facts.candidates` and a
  response warning, so a namespace or capitalization slip is corrected on the next call instead of a file listing.

### Changed

- The default import residue budget is sized for the configured `broker.max_projects`, not the compiled-in default of
  four, and never exceeds one child's share of the machine after the import in flight (`RAM / max_projects − 4.5 GiB`).
  On a 24 GiB machine with four projects the old derivation handed every child the 9 GiB floor — 36 GiB of residue with
  an 18 GiB heap ceiling each — and the machine paged until it hung; the new one hands each 1.5 GiB and says so in the
  below-floor warning. Sizing policy only: residue accounting, the derived heap ceiling, and the absence of any
  resident-size policy are unchanged.
- `ProjectRuntimeConfig::from_env_with_file` takes the pool size, `ProjectRuntimeConfig::default_for_projects` exposes
  the derivation, and startup logs the derived residue budget.

### Removed

- `lean_lookup(kind="changed_coverage")` and the `lean_verify` `changed` target group, with the `coverage` result field
  and the `unknown_coverage` summary field. No consumer used them; mapping a diff to declarations is the caller's job.

### Fixed

- `docs/operations.md` and the config catalogue described the default budget as always within 9–12 GiB. They now state
  the machine cap, the per-child worst case the pool must fit, and that the worker child inherits `LEAN_RS_NUM_THREADS`
  from the daemon environment.

## [0.12.0] - 2026-08-23

### Added

- **Added progress notifications for long `lean_verify` calls.** Requests carrying an MCP `_meta.progressToken` now
  receive start, 15-second heartbeat, and completion events on `notifications/progress`; requests without a token and
  the final verification response are unchanged.
- **Added per-call worker timing to every tool response.** The telemetry `runtime` block gains `call_elapsed_millis`
  (worker-call wallclock, queue wait excluded, including any recycle and retries the call triggered) alongside the
  existing `queue_wait_millis`, and `call_cpu_millis` (the worker child process's own CPU delta around the call,
  contention-immune, generation-guarded so a mid-call recycle reports `null` rather than a bogus figure). CPU sampling
  comes from `lean-rs-worker-parent` 0.8's `cumulative_cpu_millis`: Linux reads `/proc/<pid>/stat`, macOS uses
  `proc_pidinfo` scaled by `mach_timebase_info`.
- **Added a slow-elaboration warning.** When `call_cpu_millis` exceeds `output.slow_call_warning_millis` (default 10000
  ms, env `LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS`), every tool response carries a warning issue naming the CPU
  figure and recommending refactoring — split the declaration, add explicit type annotations to cut typeclass search, or
  extract intermediate lemmas. Keying on CPU rather than wallclock keeps machine contention from triggering it; when
  wallclock is more than twice the CPU figure the warning attributes the wait to contention instead. The warning is
  emitted in the response funnel, so it survives the default `telemetry.verbosity = quiet` drop.

### Changed

- **The lean-rs dependency line advances to 0.8** (`lean-rs-worker-parent` / `-child` / `-protocol` / `lean-toolchain`
  0.8.0, `lean-semantic-search` 0.7.3). The supported Lean window is unchanged: `4.30.0 ..= 4.34.0-rc2`.

## [0.11.0] - 2026-08-22

### Changed

- **Widened the toolchain window head to Lean 4.34.0-rc2 and added 4.33.1 to the window.** The `lean-rs` /
  `lean-semantic-search` 0.7 line is unchanged (already at lean-rs 0.7.3 / lean-semantic-search 0.7.2): lean-rs 0.7.3's
  `SUPPORTED_TOOLCHAINS` adds 4.33.1 (new `lean.h` digest backporting the 4.34.0-rc1 TSan instrumentation —
  byte-identical ABI probe) and 4.34.0-rc2 (new digest implementing the sticky-rc fix — no struct declaration changes,
  byte-identical probe against rc1), so the supported window is `4.30.0 ..= 4.34.0-rc2`. The fixture pin, the worker the
  server is built and tested against, and the head-toolchain literals in the test suite all move to v4.34.0-rc2. The
  Rust floor is unchanged at 1.94.
- **Upgraded rmcp from 1.8 to 3.1.4.** Tracks the MCP 2025-11-25 spec alignment: `Content` is renamed to `ContentBlock`,
  version negotiation now runs through rmcp's `supported_protocol_versions()` (the explicit `ProtocolVersion::LATEST`
  pin in `get_info` was dropped as redundant), and `CallToolResult` gained a `resultType` field that rmcp omits for
  legacy sessions. Newer clients may now negotiate protocol 2025-11-25, under which the Streamable HTTP transport
  enforces the `MCP-Protocol-Version` header. The Rust floor moved from 1.91 to 1.94 (which also matches the lean-rs
  0.7.3 floor).

## [0.10.0] - 2026-08-11

### Changed

- **Widened the toolchain window head to Lean 4.34.0-rc1.** The `lean-rs` / `lean-semantic-search` 0.7 line is
  unchanged; upstream lean-rs 0.7.2 added 4.34.0-rc1 to `SUPPORTED_TOOLCHAINS` (new `lean.h` digest confined to TSan
  instrumentation — byte-identical ABI probe, all 88 required symbols resolve), so the supported window is
  `4.30.0 ..= 4.34.0-rc1`. The fixture pin, the worker the server is built and tested against, and the head-toolchain
  literals in the test suite all move to v4.34.0-rc1. The Rust floor is unchanged at 1.91.

## [0.9.1] - 2026-08-10

### Fixed

- **The e2e suite raced itself under the default parallel test runner.** The two rebuild tests move the fixture's
  `Handles.olean` mtime forward to stand in for `lake build`, while the two residue-budget tests assert across
  multi-second sleeps that the worker serving that same import never recycles — one test's bump landed inside another's
  stability window and both failed with `artifacts_rebuilt` recycles that neither caused. The four tests now share a
  `serial_test` key (`handles_olean`) that confines the exclusion to exactly them; the rest of the suite still runs in
  parallel.

## [0.9.0] - 2026-08-04

### Changed

- **Adopted the lean-rs / lean-semantic-search 0.7 line and widened the toolchain window head to Lean 4.33.0-rc2.**
  `lean-rs-worker-{child,parent,protocol}`, `lean-toolchain`, and the four `lean-semantic-search-*` crates move from 0.6
  to 0.7; upstream's `SUPPORTED_TOOLCHAINS` now groups 4.33.0-rc2 with rc1 (byte-identical ABI probe), so the supported
  window is `4.30.0 ..= 4.33.0-rc2`. The fixture pin, the worker the server is built and tested against, and the
  head-toolchain literals in the test suite all move to v4.33.0-rc2. The Rust floor is unchanged at 1.91.

## [0.8.0] - 2026-07-28

### Fixed

- Derive the Lean heap ceiling from the residue budget: budget plus the larger of that budget and 8 GiB. The former
  fixed 8 GiB ceiling was below the 9 GiB residue floor, aborting healthy imports before clean recycling. Heap
  exhaustion throws a C++ exception across the Rust FFI boundary and aborts the child with exit status 134; it is not
  ordinary Lean-domain data. A three-file kan-proofs sweep aborted on file three at 1.7 GiB of residue; heap allocation
  outside the residue accounting measured 4.4–6.3 GiB. The same sweep now completes without restarts. Explicit heap
  limits at or below the residue budget produce a startup warning.
- Cap the derived residue budget at RAM remaining after one in-flight import. Its unconditional 9 GiB floor could exceed
  available memory on hosts below about 12 GiB, letting the OS kill the child before recycling.
- Include the child's exit status and stderr in `ChildAbort` restart evidence; previously only the operation survived
  supervisor handling.
- Evict pooled environments when Lean registers a new environment extension. Their fixed-size `extensions` arrays
  otherwise became too short and panicked during elaboration. Preserve reuse hints after staleness eviction so later
  pool hits do not count as imports. This requires patched lean-rs; no wire or configuration change.
- Count unrecognized `worker_internal` restarts toward `runtime.worker_restart_limit`, and map `ChildAbort` restart
  reasons to `child_abort`. Both paths previously treated abnormal restarts as planned.

### Changed

- Raise the supported Lean floor to 4.30.0 with lean-rs 0.6.1. Lean 4.27.0–4.29.1 lacks the mandatory shim's
  `collectAxioms` symbol; project opening rejects those pins and `install-worker --prune` removes their workers.
- Replace the four-import recycle limit with accumulated import-residue bytes. Only the first import maps compacted
  regions; later imports copy their whole closure privately, including overlap. Measured first imports cost 10–18 MB,
  later imports 2.0–4.5 GB; four imports cost 9.6 GiB in one workload and 16.0 GiB in another.
  `runtime.worker_import_residue_budget_mib` defaults to a quarter of system RAM divided among `broker.max_projects`,
  nominally clamped to 9–12 GiB and capped for in-flight import headroom. The `max_imports = 32` backstop applies only
  when import statistics are zero. `import_residue` is a planned restart and does not taint a verdict.
- Size the session pool independently with `runtime.worker_session_pool_capacity` (default 8). A held environment costs
  about 70 MiB versus 2.0–4.5 GB to re-import it. The old coupling recycled the child before LRU eviction could run.
- Recycle idle projects above 60% of their residue budget and re-import the latest profile between calls. The
  `import_residue_idle : import_residue` ratio shows how often recycling stays off the request path. Without a tokio
  runtime, the actor retains its blocking receive loop.
- Reuse matching worker sessions instead of importing on every call. The former growth measured about 1.18 GiB per
  import and 2.9 GiB across eleven repeated opens. Warm fixture `search_for_proof` fell from 1.41 s to 1.26 s (−10.3%, p
  = 0.01).
- Pool outgoing sessions so switching between warm profiles avoids imports. A 200-call, two-profile workload now
  performs two imports without recycling; previously it recycled roughly every other call. In `worker_roundtrip`, pooled
  alternating inspection measured 2.17 ms versus 2.36 ms steady; a switch previously required about 787 ms of
  respawn/re-import. Raise the supervisor's all-cause restart backstop above the planned cycle rate; its former
  16-per-minute default could permanently refuse replacements.
- Remove RSS-based policies: `worker_rss_post_job_restart_kib`, `worker_rss_hard_kill_kib`, `worker_rss_sample_millis`,
  `import_switch_rss_soft_kib`, their validation and the forced recycle every 64 requests. RSS includes healthy shared
  `.olean` mappings. The derived Lean heap ceiling is an abort backstop above the residue budget, not a graceful error
  limit. Removing the hard-kill watchdog also removes macOS `/bin/ps` polling every 250 ms. RSS remains once-per-call
  telemetry and drives no policy.
- Track rebuilt artifacts across every profile the child may still hold, bounded by session-pool capacity. Returning to
  a pooled profile now detects an intervening `.olean` rebuild at the same cost of one `stat` per import per call.
- Avoid the unused startup session when a worker builder receives no imports. Builders given imports still open eagerly.
  Deployment validation through `LeanWorkerHostHandleBuilder::check` remains unchanged.
- Rename `rss_threshold_sweep.py` to `memory_stability.py` and replay workloads against one server. Report residue bytes
  and limits in telemetry and `peak_import_residue_mib`, `final_import_residue_mib`, `import_residue_budget_mib` in the
  harness summary. A 60-call Mathlib-scale workload returned 60/60 `ok`, no generation change or restarts, and flat
  residue at 1786 MiB against a 9216 MiB budget. Add a kan-proofs workload alongside the fixture workload.
- Reduce project-scope reference parsing: one parse on success, separate declaration/reference projections and filtering
  during deserialization. In a 1431-module, 76 MB `.ilean` corpus, parsing accounted for about 95% of runtime; the old
  path parsed twice and allocated about 400,000 transient entries. The version field appeared at byte 116,287 of 116,299
  in one file, so a supposed prefix probe scanned the whole document. Worst-case scan time fell from 427 ms to about 120
  ms without a persistent cache. Failed parses still re-probe the version to preserve future-version errors.
- Read only contributing indices for `files`-restricted reference queries: one-file time fell from 271 ms to about 0.5
  ms. Uncertain path inversion, including mangled names such as `«kan-lint-style»`, falls back to a full walk.

### Added

- `worker_roundtrip` gains a pooled alternating-import arm. `import_switch` distinguishes steady, pooled alternating and
  pool-overflow profiles. Its earlier, pre-pooling baseline measured 4.9 ms steady versus 555 ms alternating; those
  measurements predate the pooled `worker_roundtrip` figures above.
- `ilean_reference_scan` measures full, no-hit and narrowed project scans. The former approximate 565 ms record was a
  single ignored-test sample on a corpus one-third the size, rather than a comparable benchmark.
- Recycle reused sessions when imported `.olean` mtimes advance, reported as `artifacts_rebuilt`. This uses one `stat`
  per import instead of a whole-tree scan, which measured 160–190 ms warm over Mathlib's 8408 `.olean` files.
- `lean_status` reports `broker.lean_max_memory_kib`.
- `cache_hit` measures warm `proof_state`: 57.6 µs unchanged versus 21 ms after a one-comment edit invalidates the hash.
  The uncached `module_query_roundtrip` measures an 18–20 ms worker call.

### Additional fixes

- Run project-scope reference file I/O and JSON parsing outside tokio worker threads.
- Preserve `telemetry.verbosity = full` through the semantic response boundary; the telemetry block previously vanished
  at both verbosity levels. Quiet responses still omit it.
- Server-spawning integration tests select their adjacent built worker instead of an unrelated installed image.
  `stdio_lifecycle` also resolves release workers correctly instead of hardcoding `target/debug`.
- Update the memory harness to semantic tool names, both response carriers and `telemetry.runtime`/`trust`, and request
  full telemetry. It previously reported null memory/restart facts as though it had measured stability.

## [0.7.0] - 2026-07-24

### Added

- `lean_trial(kind="proof_step")` envelopes are self-contained: every batch result carries the entry goals and local
  hypotheses of the selected proof position (`entry_goals` / `locals`), rendered once per envelope through the existing
  rendering machinery. A proof-stepping trial loop no longer needs a per-step `lean_context` call; boundaries stay
  available as a one-shot navigation call.
- `retry_tainted_non_positive` (default `false`) on `lean_trial` and `lean_verify` opts into one server-side retry of a
  non-positive verdict when the worker was recycled mid-call (tainted by the RSS watchdog). With the flag off, behavior
  is byte-for-byte unchanged: the taint is still reported via `execution_taint` and the existing
  relabel-to-`worker_recycled` policy.
- Batch trial results gain `post_closure_diagnostics`: error-severity entries are moved off `closed` candidates, so a
  `closed` candidate's own diagnostics are error-free and the post-closure consequences (e.g. `no goals` from a
  follow-on tactic) travel in their own bucket.
- By-name verification and the declaration outline now resolve every surface declaration form — multi-clause equation
  `def`s and theorems, `where`-structure defs, `structure`/`class` commands, and anonymous `instance`s under their
  generated `inst…` names — via the lean-rs 0.5.0 shim's declaration-candidate scan repair. `not_found` now means the
  name is genuinely absent.

### Changed

- **Breaking:** `lean_context` drops the declaration echo fields from its default response and makes the boundary list
  and `expected_type` opt-in (`include_boundaries`, `include_expected_type`, both default `false`). The trimmed default
  response is roughly half the old size; set the flags for the old shape.
- Adopted the `lean-rs` 0.5 line (`lean-rs-worker-{child,parent,protocol}` and `lean-toolchain` 0.4 → 0.5) and the
  `lean-semantic-search` 0.5 stack; the supported Lean window is unchanged (`4.26.0 ..= 4.33.0-rc1`).
- Completeness flags that equal their default are omitted from MCP responses instead of serialized (`verified`,
  `truncated: false`, `tainted: false`, zero counts, empty arrays, absent axiom facts), so absent unambiguously means
  "complete / nothing to report".

## [0.6.0] - 2026-07-19

### Changed

- Adopted the `lean-rs` 0.4 line (`lean-rs-worker-{child,parent,protocol}` and `lean-toolchain` 0.3 → 0.4, alongside the
  already-current `lean-semantic-search` 0.4 stack), widening the supported window to `4.26.0 ..= 4.33.0-rc1` via
  `lean-toolchain` 0.4 → `lean-rs-abi::SUPPORTED_TOOLCHAINS`.
- Moved the head toolchain the server is built and tested against from `leanprover/lean4:v4.32.0` to
  `leanprover/lean4:v4.33.0-rc1` (fixture pin, the `src/` head literals, and the README "Versions" matrix / doc JSON
  examples). No Rust-floor change.
- Adapted the `SourceRanges` Lean fixture to the `v4.33.0-rc1` `Environment.addDeclCore` signature, which gained a
  `maxRecDepth : USize` parameter after `maxHeartbeats` (`env.addDeclCore 0 decl none` →
  `env.addDeclCore 0 0 decl none`, mirroring core Lean's own call site).

## [0.5.1] - 2026-07-14

### Changed

- Moved the head toolchain the server is built and tested against from `leanprover/lean4:v4.31.0-rc2` to
  `leanprover/lean4:v4.32.0` (fixture pin and the `src/` head literals), landing on the final release after an
  intermediate stop at `v4.32.0-rc1`. No dependency bump was required: the already-adopted `lean-rs` 0.3 /
  `lean-semantic-search` 0.4 lines (via `lean-toolchain` 0.3 → `lean-rs-abi::SUPPORTED_TOOLCHAINS`) already cover
  `4.32.0`, extending the supported window to `4.26.0 ..= 4.32.0`. Also refreshed the README "Versions" matrix, which
  had lagged at the pre-0.5.0 `lean-rs` 0.2.2 line.
- Bumped the parent crate's `rmcp` dependency from 1.7 to 1.8.

## [0.5.0] - 2026-06-19

### Added

- New semantic MCP surface with five public tools: `lean_context`, `lean_trial`, `lean_verify`, `lean_lookup`, and
  `lean_status`. The surface carries stable `data` / `errors` / `trust` responses, preserving proof-relevant artifact
  freshness facts even when telemetry is quiet.
- `lean_verify` batch target groups for explicit declarations, all declarations in a file or module, and changed-target
  verification. Changed verification reports conservative coverage gaps for unmapped hunks, deleted files, renames, and
  truncation instead of silently dropping edits.
- `lean_lookup(kind = "declarations")` for source-fresh declaration inventory with `.ilean` build-fresh fallback, and
  `lean_lookup(kind = "changed_coverage")` for changed-hunk-to-declaration mapping without verification.
- `lean_trial(kind = "command")` for bounded non-mutating command snippets such as `#check` and `#print axioms`, and
  `lean_status(kind = "file_diagnostics")` for current-source Lean diagnostics.
- Runtime lifecycle hardening for server shutdown, queued-job terminal outcomes, process registry diagnostics, and
  stale-record cleanup that only removes host-owned dead PID records.

### Changed

- Upgraded the `lean-rs` worker crates (`lean-rs-worker-parent` / `-child`, `lean-toolchain`) to 0.3 and the
  `lean-semantic-search` crates to 0.4 (which themselves build on `lean-rs` 0.3), so the entire dependency graph now
  resolves to a single `lean-rs` 0.x line. The Lean toolchain pin remains `leanprover/lean4:v4.31.0-rc2`.
- Public MCP registration is now the semantic five-tool surface. The old six proof-workflow tool names and temporary
  compatibility probes are not advertised; existing internal operation modules are reused behind semantic modes.

### Internal

- Centralized the `lean-rs` and `lean-semantic-search` dependency versions in `[workspace.dependencies]` (members
  reference them via `.workspace = true`) and added a `cargo-deny` CI job whose `deny.toml` floor-bans the `lean-rs`
  crate family below `0.3`. A future partial upgrade that drags a pre-0.3 copy back into the graph now fails
  `cargo deny check` with a named diagnostic instead of a deep `E0308` type mismatch.

## [0.4.1] - 2026-06-09

### Fixed

- `search_for_proof` semantic-lane candidates now return the clean Lean name (e.g.
  `FirstOrder.Language.BoundedFormula.IsDelta0.bdAll`) with the `module` field populated, instead of leaking the
  downstream `origin:module:declName` corpus key as the `name` (e.g.
  `lean-host-mcp:KanProofs.ModelTheory.Delta0.Basic:FirstOrder…bdAll`). The prefixed key broke the documented key-free
  contract and the `search_for_proof` → `inspect_declaration` handoff.

## [0.4.0] - 2026-06-09

### Added

- `search_for_proof` now has a semantic retrieval lane for file/declaration-backed queries. When a request includes
  source context, the tool asks `lean-semantic-search` for proof-goal features, extracts declaration features from the
  candidate modules, ranks semantic candidates, and merges them with the existing declaration-search fallback. Public
  evidence stays key-free: semantic matches surface as stable `semantic:*` `match_reason` labels such as
  `semantic:role_conclusion_const` or `semantic:conclusion_fingerprint`; raw feature keys and capability command names
  stay private. If the semantic command is unavailable or fails, the tool returns the existing structural fallback with
  a warning instead of failing the MCP call. The capability now comes from the package-owned
  `lean-semantic-search-runtime` crate, so consumer projects do not expose or import `LeanSemanticSearch.Capability`.
- Cross-process admission control for semantic/elaborating work. Parallel server processes sharing a lock directory now
  coordinate before running heavy worker calls, so semantic proof search and other elaborating requests do not stampede
  the machine. New `[broker]` knobs and env overrides: `semantic_permits` / `LEAN_HOST_MCP_SEMANTIC_PERMITS`,
  `semantic_waiters` / `LEAN_HOST_MCP_SEMANTIC_WAITERS`, `semantic_admission_timeout_millis` /
  `LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS`, and `semantic_lock_dir` / `LEAN_HOST_MCP_SEMANTIC_LOCK_DIR`.
  Saturation returns retryable structured errors such as `semantic_admission_full` or `semantic_admission_timeout`.

### Changed

- Bumped the `lean-semantic-search-*` crates to 0.3.0 and adapted to the storage-neutral retrieval API
  (`retrieve_across(&[&dyn Corpus], ...)` instead of `SemanticIndex::retrieve(...)`). `lean-semantic-search-runtime` is
  now published, so the host consumes it from crates.io; the temporary local-path `[patch.crates-io]` override is gone.
- Bumped `lean-rs-worker-parent` and `lean-rs-worker-child` to 0.2.0 and `lean-toolchain` to 0.2.1. This pulls link-free
  `lean-rs-abi` metadata into the parent-side dependency graph, so `cargo nextest run --workspace --no-fail-fast` no
  longer builds the parent test binary with an accidental `libleanshared` load command through workspace feature
  unification. The worker crate still links `libleanshared`; the parent crate remains link-free.
- Updated the host for additive `lean-rs` 0.2.0 telemetry fields: RSS/resource fields are ignored where the envelope
  does not yet expose them, declaration inspection leaves the new proof-search facts disabled by default, and module
  query cache-fact fixtures set `resource: None`.

### Fixed

- Workspace-wide nextest no longer aborts while listing parent tests on macOS with
  `Library not loaded: @rpath/libleanshared.dylib`; the parent test binary is again free of Lean runtime linkage when
  built together with the worker.

## [0.3.0] - 2026-06-02

### Changed

- `find_references` at `scope: "project"` now reads Lean's on-disk `.ilean` reference index instead of elaborating every
  `.lean` file through the worker. The old path issued one worker module-query per file (~3 s/file → ~27 min on a
  ~500-file project) and only returned an arbitrary truncated prefix when it hit the request-time budget. The index read
  is sub-second, returns the **complete** result (only the `limit` cap truncates, on a stable sorted prefix), and
  involves the worker not at all. Project-scope results are now **build-fresh** — they reflect the last `lake build` —
  while `file` scope stays on the worker path and remains **edit-fresh** (current source); the asymmetry is documented
  in the tool catalog. An unbuilt project degrades to the existing `needs_build` top-level warning (not an empty "no
  references", not a hard error), and an index stale relative to current source rides a freshness note. The
  `files_scanned`/`files_skipped` counters now report `.ilean` modules indexed / skipped at project scope. The now-moot
  per-file wall-clock scan deadline and its "hit the request time budget" warning are gone.
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
  re-running it after a `lean-host-mcp` upgrade brings every worker back in step. `--force` rebuilds current workers
  too.
- `install-worker --clean [--toolchain <id>]` removes all installed workers (or one); `install-worker --prune` removes
  only unservable workers (outside the supported window, or with a failed smoke test), keeping servable-but-stale ones.
  Both are idempotent and only touch the install root.
- Configurable per-request timeout: `runtime.request_timeout_millis` (env `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS`),
  default **120 s**. Replaces the worker's fixed 10-minute long-running profile. On expiry the worker is recycled and
  the call returns a retryable runtime error; raise it for unusually heavy modules, lower it to bound calls.
- `find_references` at project scope is now bounded by that same budget as an overall wall-clock deadline, not just per
  file. It fans one worker query out across every module in the project (hundreds in a large tree), so the per-request
  timeout alone left the aggregate sweep able to run for many minutes and appear to hang. The deadline now runs
  concurrently with each in-flight query, so even a single stalled module cannot block the call; whatever was indexed
  before the deadline is returned with a truncation warning and a cue to narrow with `files` or raise the budget.

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

[Unreleased]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.19...HEAD
[0.16.19]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.18...v0.16.19
[0.16.18]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.17...v0.16.18
[0.16.17]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.16...v0.16.17
[0.16.16]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.15...v0.16.16
[0.16.15]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.14...v0.16.15
[0.16.14]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.13...v0.16.14
[0.16.13]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.12...v0.16.13
[0.16.12]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.11...v0.16.12
[0.16.11]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.10...v0.16.11
[0.16.10]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.9...v0.16.10
[0.16.9]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.8...v0.16.9
[0.16.8]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.7...v0.16.8
[0.16.5]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.4...v0.16.5
[0.16.4]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.3...v0.16.4
[0.16.3]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.2...v0.16.3
[0.16.2]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.16.1...v0.16.2
[0.16.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.15.0...v0.16.1
[0.15.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.14.2...v0.15.0
[0.14.2]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.14.1...v0.14.2
[0.14.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.14.0...v0.14.1
[0.14.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jcreinhold/lean-host-mcp/releases/tag/v0.1.0
