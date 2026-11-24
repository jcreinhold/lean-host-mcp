# Operations

Configure and troubleshoot the server here. Start with the [README](../README.md) for installation and client setup; use
the [tool catalog](tool-catalog.md) for request and response contracts.

- [Configuration](#configuration-file) and [reference table](#configuration-reference)
- [Artifact freshness](#rebuilt-artifacts) and [memory sizing](#memory-and-import-cycling)
- [Concurrency and admission](#concurrency-and-admission)
- [Logging](#request-activity-in-the-terminal) and [worker recycles](#observing-worker-recycles)
- [Process lifetime](#process-lifetime) and [runtime failures](#runtime-error-contract)
- [Worker installation](#installing-workers) and [test harnesses](#build-test-lint)


Worker lifeline sockets use `LEAN_HOST_MCP_SOCKET_DIR`, then `$XDG_RUNTIME_DIR/lean-host-mcp` when XDG is set,
otherwise `$HOME/.lean-host-mcp-ipc`. Choose a short canonical directory owned by the server user with mode 0700.
The SDK removes each worker's socket on closure. Durable owner records continue to use their configured registry.
The host passes `LEAN_RS_WORKER_OWNERS_DIRECTORY` to its SDK helpers. The default is
`lean-host-mcp/native-generations` under the platform's local data directory. On macOS this is
`$HOME/Library/Application Support/lean-host-mcp/native-generations`. Keep the same canonical directory when
restarting the server, including across launcher mode and binary changes. `LEAN_HOST_MCP_PROCESS_REGISTRY_DIR`
sets the separate server PID directory used by `doctor processes`.

The SDK writer executable setting is `LEAN_RS_WRITER_EXECUTABLE`. Launch environments may still use
`LEAN_HOST_MCP_WRITER_EXECUTABLE` during the transition. A startup that supplies both must supply identical values.
The server still selects its embedded writer for ordinary operations and honors explicit standalone-writer selections.

## Configuration file

Generate a commented TOML file with the current defaults:

```sh
lean-host-mcp config init          # ./lean-host-mcp.toml
lean-host-mcp config init --home   # platform user config directory
```

`--path FILE` selects another destination. Existing files require `--force` to overwrite.

Startup loads `<config-dir>/lean-host-mcp/config.toml`, then overlays the nearest `lean-host-mcp.toml` found by walking
upward from the server's working directory. Files merge per key, so a local file need only set its overrides.
`LEAN_HOST_MCP_CONFIG_DIR` replaces the platform config directory. Missing files are ignored; parse failures are logged
and skipped. A merged value with the wrong type causes the merged file configuration to be ignored.

## Configuration reference

For each setting, precedence is **CLI flag > environment variable > configuration file > built-in default**. Units are
shown in the table; GiB figures in descriptions are explanatory, not literal setting values.

<!-- BEGIN GENERATED: do not edit by hand. Regenerate from `config_schema::render_reference_table`; the `operations_md_reference_table_is_in_sync` test fails when this block drifts. -->
| Key | Type | Default | Override | Description |
| --- | --- | --- | --- | --- |
| `primary_project` | path | unset | `--lake-root / LEAN_HOST_MCP_PROJECT` | Default Lake project for calls that omit an explicit project= argument. Lowest-priority fallback, after the flag/env and the nearest lakefile above the working directory. |
| `runtime.lean_max_memory_kib` | integer (KiB) | unset | `LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB` | Lean heap ceiling per worker child. Allocator exhaustion can abort the child when a C++ exception reaches the Rust FFI boundary. Unset derives the residue budget plus max(residue budget, 8 GiB), converted to KiB, leaving headroom above clean import recycling. Startup warns when an explicit ceiling is at or below the residue budget. |
| `runtime.request_timeout_millis` | integer (ms) | `120000` | `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS` | One shared budget for worker-capacity admission, mailbox admission and queued waiting for each worker operation. After dispatch, worker execution has a separate timeout of the same length; execution expiry recycles the worker. Admission expiry does not dispatch that queued operation. This is not a whole-tool deadline: preparation and multiple operations can take longer. Default 120 s. |
| `runtime.project_mailbox_capacity` | integer | `16` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | Maximum queued calls in each worker actor's mailbox. Further callers wait in FIFO order for mailbox space within the same original request_timeout_millis admission deadline. Cancellation removes only that caller. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many worker restarts are tolerated within the restart window before the project is marked unhealthy. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `runtime.worker_import_residue_budget_mib` | integer (MiB) | unset | `LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB` | Unreclaimable import bytes per worker before a clean recycle. Unset uses a quarter of RAM divided among broker.max_workers, clamped to 9-12 GiB and capped at each worker's RAM share minus 4608 MiB (minimum 1 MiB). This affordability cap can lower the default below 9 GiB. Explicit MiB values bypass RAM sizing; a budget below one import's residue can force frequent recycling. |
| `runtime.worker_session_pool_capacity` | integer | `8` | `LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY` | How many imported Lean environments one worker child keeps warm. Returning to a pooled import profile costs a key comparison instead of a full import, so this should cover the number of distinct per-file import profiles a session moves between — a changed-file sweep touches one per file. A held environment costs about 70 MiB against 2-4 GB for the import it saves. Default 8. |
| `broker.max_workers` | integer | `4` | `LEAN_HOST_MCP_MAX_WORKERS (legacy: LEAN_HOST_MCP_MAX_PROJECTS)` | Maximum worker processes across all projects, including startup and shutdown. Idle workers may be evicted to serve another project. max_projects is a deprecated alias; environment values override file values, and the new name wins within each layer. |
| `broker.max_workers_per_project` | integer | `1` | `LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT` | Maximum independent workers for one canonical project root. Extra workers open only with spare global capacity; otherwise requests queue on an existing worker. Set 2 only after measuring the memory cost of concurrent checks. Each worker retains its own imports and cache. |
| `broker.idle_timeout_secs` | integer (s) | `600` | `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Evict a project's worker after this many idle seconds. 0 disables idle eviction. Default 10 minutes. |
| `server.bind` | string (loopback ADDR:PORT) | unset | `--bind / LEAN_HOST_MCP_BIND` | Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS. |
| `server.http_path` | string | unset | `--http-path / LEAN_HOST_MCP_HTTP_PATH` | HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp. |
| `server.response_carrier` | string (text, structured, both) | `"text"` | `LEAN_HOST_MCP_RESPONSE_CARRIER` | Which field of the tool result carries the semantic response. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text. |
| `telemetry.verbosity` | string (quiet, full) | `"quiet"` | `LEAN_HOST_MCP_TELEMETRY_VERBOSITY` | How much operational telemetry the internal operation envelope keeps before semantic response adaptation. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet. |
| `output.max_field_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES` | Override the per-field output byte cap for all tools. Unset keeps each tool's built-in default (8 KiB for inspection, 4 KiB for proof actions). Clamped to 256 bytes to 64 KiB. |
| `output.max_total_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES` | Override the total output byte cap for all tools. Unset keeps the built-in 64 KiB default. Clamped to 1 KiB to 64 KiB. |
| `output.heartbeat_limit` | integer (heartbeats) | unset | `LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT` | Default elaboration heartbeat budget for lean_trial proof_step and lean_verify target groups. Unset uses the worker default. Bounds runaway tactics. |
| `output.slow_call_warning_millis` | integer (ms) | `10000` | `LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS` | Worker-call CPU time above which every tool response carries a warning naming the elapsed milliseconds and recommending refactoring (splitting the declaration, explicit type annotations, extracted lemmas). Keys on call_cpu_millis, not wallclock, so machine contention never triggers it. Default 10 s. |
<!-- END GENERATED -->

`output.max_field_bytes` and `output.max_total_bytes` bound model-facing payloads before they leave the worker. Tight
caps are useful for smoke tests and for very large proof states, but they can also make proof-step batches partial: once
the total proof-action budget is exhausted the current candidate reports `budget_exceeded` and later candidates report
`not_attempted`. Retry the promising snippet alone, or raise `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES`, when the response
summary shows nonzero `budget_exceeded`, `not_attempted`, or `output_truncated` counts.

### Rebuilt artifacts

A worker session's Lean environment is a snapshot taken at import. Reusing that session does not read newer `.olean`
files, so the server must detect build changes before reusing it.

The server records individual imported `.olean` metadata, including resolved path, size, modification time, and Unix
file identity/change time. It follows manifest package order, custom `packagesDir`, path dependencies, Git `subDir`, and
inherited `LEAN_PATH`. Rebuilt, removed, replaced, or newly available artifacts invalidate affected cached queries and
pooled sessions. The worker is recycled before reuse; `artifacts_rebuilt` records that transition. Unrelated artifacts
do not invalidate a warm query.

Only named imports are watched rather than their full transitive closure: normal Lake dependency propagation must update
the corresponding imported artifacts. Bundled shims and installed toolchain contents remain trusted fixed runtime
inputs. Concurrent builds are not an atomic execution snapshot; changed artifacts prevent result-cache insertion.
Callers requiring an exact source/build revision must coordinate those inputs.

### Memory and import cycling

Two quantities govern worker memory:

- `runtime.worker_import_residue_budget_mib` bounds retained import bytes and triggers a clean recycle.
- `runtime.lean_max_memory_kib` is a Lean heap backstop. Crossing it can abort the entire child: Lean's allocator throws
  a C++ exception that cannot unwind through the Rust shim frame. It is not reliably a recoverable elaboration error.

RSS is telemetry only. It includes clean, shared, memory-mapped `.olean` pages and does not measure unreclaimable
imports; no server policy restarts, refuses, or recycles a worker on RSS or call count.

An environment imported with `loadExts := true` retains its imported regions until the child exits. Only the first
import in a process memory-maps compacted regions; later imports materialize their full closure as private copies,
including overlapping modules. On measured Mathlib-scale per-file headers, the first import retained 10–18 MB and each
later import 2.0–4.5 GB. Four imports cost 9.6 GiB on one workload and 16.0 GiB on another, so an import count does not
predict the required memory. The parent sums the child's `non_memory_mapped_region_bytes` instead.

The child keeps up to `runtime.worker_session_pool_capacity` imported sessions (default 8). Returning to a held profile
restores it without importing. In the same measurements, holding another environment cost about 70 MiB against 2–4 GB
for the import it saved. Session eviction and residue recycling are separate policies: dropping an environment does not
reclaim its imported regions. A count backstop of 32 applies only when the child reports unusable zero-valued import
statistics.

The default residue budget starts at a quarter of system RAM divided by `broker.max_workers`, clamped to 9–12 GiB. It is
then capped at one worker's share of RAM minus 4.5 GiB for an import in flight; this affordability cap can put it below
9 GiB. `broker.max_projects` is the legacy name for the same global worker cap.

The default heap ceiling is **budget + max(budget, 8 GiB)**. At budgets of at least 8 GiB this doubles the budget; at
smaller budgets the fixed gap preserves room for Lean allocations outside the residue counter. The measured gap was
4.4–6.3 GiB during an abort under the former fixed 8 GiB ceiling. That ceiling aborted a three-file sweep on file three
before a 9 GiB residue budget could fire. Set the heap ceiling explicitly only with this ordering in mind; startup warns
when a configured ceiling is at or below the residue budget.

Size the worker count first. A conservative envelope per worker is `budget + 4.5 GiB + max(budget, 8 GiB)`, alongside
the rest of the machine's workload. For example, one worker with an explicit 4 GiB residue budget has a derived 12 GiB
heap ceiling; its conservative envelope is 16.5 GiB. An explicitly fixed budget does not shrink when the worker count
changes. `LEAN_RS_NUM_THREADS` on the server also bounds inherited Lean task threads; the default uses every core, and
concurrent elaboration tasks add working sets.

For stable artifacts, the import policy behaves as follows:

- Repeating one file reuses its session without adding residue.
- Alternating among profiles still held in the session pool also adds no residue.
- Sweeping new profiles eventually recycles the child. Frequency depends on each import's measured residue and the
  budget, rather than the number of tool calls.

`import_residue` records a reactive recycle on a request. Above 60% of the budget, a quiet tick can instead recycle and
re-import the most recent profile; this is `import_residue_idle`. A higher idle-to-reactive ratio means more of the
restart cost lands between calls. Reactive cycles dominate when work never pauses long enough; raising the budget may
help if the machine has room. Both causes are planned, do not count toward the crash-loop limit, and do not taint a
completed verdict.

Earlier resident-memory measurements (+1 GiB per import and an 11.2 GiB `SIGKILL`) used a fixture whose import profiles
shared about 99% of their closure. They should not replace the physical-footprint measurements above: under pressure,
RSS can fall while unreclaimable import bytes keep growing.

## Concurrency and admission

Each worker owns a child process and a dedicated thread that runs one job at a time. `broker.max_workers` caps workers
across all projects. `broker.max_workers_per_project` defaults to one. With a larger per-project limit, the broker
prefers an idle worker, starts another only with spare global capacity, or queues on the least-loaded existing worker.
Each worker keeps its own import and result caches. The old `max_projects` setting is an alias for the global cap, not a
separate project quota. Both environment aliases override either file key; the new name wins within the same layer.
`runtime.project_mailbox_capacity` bounds the actor's mailbox. Further callers wait for mailbox admission in FIFO order
rather than retrying `mailbox_full`. Cold projects also wait in FIFO order for worker capacity; the head waiter retires
an idle LRU worker when one becomes available. Active workers are never evicted to make room. Different resident
projects can still execute concurrently. Cancellation removes only its own waiter, and capacity remains reserved until
worker exit is confirmed.

Worker-capacity admission, mailbox admission, and time waiting in the actor queue share one
`runtime.request_timeout_millis` deadline. Expiry returns `project_admission_timeout`, `mailbox_admission_timeout`, or
`queue_wait_timeout` without dispatching that queued operation. Once dispatch begins, the worker execution timeout
applies separately. This is not a whole-tool deadline: a tool that submits several worker operations can take longer.
Explicit retained requests share a 128-record limit; synchronous requests retain their normal transport behavior.
Parallel server processes remain independent; lower `broker.max_workers` when the machine cannot hold that many workers.

Cheap metadata paths never open a worker at all: degraded `needs_build` responses, invalid-request responses, and
project-scope `.ilean` reference reads answer from the Lake files directly. A repeat module query on an unmodified file
is answered from the per-project result cache and likewise never enters the mailbox, so a warm call does not queue
behind in-flight Lean work.

Semantic proof ranking briefly opens a companion child in addition to the elaboration worker. Include that peak in
memory planning. The server shuts it down after the call; it does not keep a separate resident ranking pool.
Measurements in `project.rs::open_semantic_capability` found no significant improvement from keeping it resident.

### Retained request storage

Enable durable request evidence with `--request-journal DIR` or `LEAN_HOST_MCP_REQUEST_JOURNAL`. Use one directory per
broker. Without it, retention is process-local. Recovered requests preserve evidence and are never replayed
automatically. See the [retained lifecycle contract](tool-catalog.md#prepared-retained-lifecycle-v3) for ticket binding,
result retrieval, acknowledgment, capacity, and recovery semantics.

## Request activity in the terminal

Default INFO logging summarizes each actual semantic execution with a server-generated `request` number, tool, canonical
kind and bounded project basename. The received line means preparation or queueing has begun, not that a worker has
already started. The matching response line includes elapsed time (including queue wait), a fixed outcome label, an
allowlisted `result_status` when present (such as `not_found` or `unsupported`), tool issue counts, and diagnostic or
verification counts when available. `completed` means a response is ready, not that a theorem was proved; inspect
`failed`, `diagnostic_errors`, and the actual client response for the verdict.

```text
INFO Lean request received (preparing or waiting for worker) request=42 tool="lean_verify" kind="verification" project=proofs
INFO Lean response ready request=42 tool="lean_verify" kind="verification" project=proofs elapsed_ms=1532 outcome="completed" tool_errors=0 tool_warnings=0 verified=3 failed=1 needs_build=0
```

Synchronous calls, negotiated MCP tasks and retained `lean_request` submissions share this boundary. Retrying the same
retained ID or polling its result does not log a second execution. Routine `lean_runtime` status and result polling
remain quiet. If a future disappears before it returns, a warning explicitly says worker completion is not confirmed;
that log is not a receipt proving that an executing worker stopped.

These summaries do not print command/tactic text, theorem names, imports, diagnostics, response bodies, caller-supplied
request IDs or full project paths. Project labels are bounded and terminal-safe; kind/outcome labels are allowlisted.
Existing detailed debug/runtime logs are separate, and may contain paths or diagnostic details.

The default filter, also used for `RUST_LOG=info`, is `info,rmcp=warn`: routine SDK session creation, initialization and
stream closure are hidden, but SDK WARN/ERROR remain visible. To investigate transport details explicitly, use
`RUST_LOG=info,rmcp=debug`. Other explicit `RUST_LOG` directives retain their usual meaning. Logs go to stderr, never
the stdio protocol stream.

The import-budget sizing estimate appears only at DEBUG, once when the broker starts, not on worker restarts. Its 9216
MiB comparison is a Mathlib-oriented sizing baseline for retaining two large imports, not a required minimum for every
import or evidence of a failure. An explicitly configured smaller budget can be intentional; evaluate actual recycle
frequency before changing it. Lowering the project count does not change an explicitly fixed budget. The heap-ceiling
warning and real runtime error/recycle logs remain intact; no memory policy changes with logging.

## Observing worker recycles

Every recycle is logged to stderr and counted in worker runtime facts. Set `telemetry.verbosity = "full"` to include
those facts in public responses under `telemetry.runtime`; they are omitted at the default quiet verbosity.

Log lines carry structured fields — `cause`, `reason`, `worker_generation`, `rss_kib`, `limit_kib`, `planned`,
`restarts_total`. Level tracks the signal, not whether the cycle was planned:

- `warn` — abnormal/crash causes: `rss_hard_limit_exceeded`, `child_abort`, `child_exit`, `session_missing`,
  `worker_internal`, `timeout`, `cancelled` (and `restart limit exceeded; marking project unhealthy`).
- `info` — `artifacts_rebuilt` (a `.olean` among the imports of a session the child may still hold was rewritten, so the
  child was recycled), `import_residue` (the reactive half of the residue budget — the cycle a call had to wait for, and
  the frequency an operator tunes against), memory-pressure cycles (`rss_post_job`), plus `opened project` / `idle
  reaper evicted projects` lifecycle lines.
- `debug` — pure hygiene (`import_residue_idle`, `max_imports`, `max_requests`, `idle`, `explicit`), detailed project
  resolution, and the `job` span.

The server no longer configures any resident-memory threshold, so in practice you see the abnormal causes above,
`explicit`, `artifacts_rebuilt`, and the two residue causes (see
[memory and import cycling](#memory-and-import-cycling)). `max_imports` should not appear at all — it is a backstop for
a child that reports no import statistics. `rss_post_job` / `rss_hard_limit_exceeded` and `max_requests` stay in the
vocabulary because the supervisor still reports them if a future policy asks for them; seeing one today means the worker
was configured somewhere other than here.

Default level is `info`; set `RUST_LOG=lean_host_mcp=debug` for internal runtime detail. Example at default level:

```text
INFO worker recycled (imports rebuilt on disk) cause=artifacts_rebuilt worker_generation=2 planned=true restarts_total=1
```

Full telemetry exposes the final-attempt cause in `call_restart`, the most recent in `last_restart`, and the lifetime
frequency in `restarts_total` plus the per-cause breakdown `restarts_by_cause` (omitted when no recycle has happened).
`import_residue_bytes` and `import_residue_limit_bytes` report the quantity the recycle policy actually reads, so how
close a child is to its next cycle is observable rather than inferred. The two figures a cycle happened *on* are in the
restart event's `reason` string.

If a worker attempt fails and a fresh retry completes, the result remains usable. The response includes a warning about
the first failure. Full telemetry records `previous_attempt_failure`, `retry_count`, and `final_attempt: "completed"`. A
failed or uncertain retry still returns no usable result.

## Process lifetime

The idle reaper (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`) governs resident per-project controllers, not the parent server
process. A stdio server exits when its transport closes: it serves until the client closes the server's stdin. It also
watches the process that launched it and exits if that parent PID disappears or changes without a clean MCP shutdown. An
HTTP server is separate: it exits on Ctrl-C, SIGTERM, or ordinary process shutdown. Both transport exit paths call
`ProjectBroker::shutdown_all`, which closes resident projects before the process returns.

Project shutdown is bounded by the worker layer. The host stops accepting new project work, queued messages receive
`runtime_unavailable` with reason `project_shutting_down`, and the controller then lets `lean-rs-worker-parent` perform
its structured child shutdown: terminate, bounded graceful wait, kill escalation if needed, and reap. An active request
may finish normally or run until the configured request timeout before the worker layer reports a terminal runtime
outcome. Abrupt parent death can still skip Rust `Drop`; child-side parent-loss handling is best effort, and stronger
containment remains a launcher or process-manager responsibility.

Every running server writes a PID record under the per-user cache directory at `lean-host-mcp/processes/` and removes it
on normal shutdown. The record contains the exact server PID, executable path, working directory, transport, bind/path,
startup parent PID, and process group. Inspect records with:

```sh
lean-host-mcp doctor processes
```

The stdio parent watcher uses the kernel's parent PID rather than `ps`, and only confirmed parent death or reparenting
ends the session. Observation failures never trigger shutdown.

The output lists only host-written records: PID, liveness, executable-match status when the platform exposes it,
transport, bind/path, working directory, startup/current parent PID, process group, stale-stdio-client status, and
direct child PIDs. Liveness is `true`, `false` (kernel-confirmed absence), or `unknown`; `liveness_error` identifies
permission denial or another observation failure. Missing process metadata and child lists remain unknown, not evidence
of absence. It does not scan for process names. Only a confirmed live stdio server with a matching executable and a
confirmed parent change receives an exact PID suggestion such as `kill -TERM <pid>` for the recorded server PID. Clean
records left behind by abruptly killed servers with:

```sh
lean-host-mcp doctor processes --cleanup-stale-records
```

Cleanup removes records only after kernel-confirmed PID absence is rechecked; unknown observations preserve records. It
does not kill live processes and does not infer ownership from an executable name, command substring, or port number. Do
not use broad process-name cleanup; identify the exact PID from the registry or from
`ps -axo pid,ppid,pgid,stat,rss,command` before terminating a live process.

## Runtime-error contract

Recoverable runtime and controller failures return a semantic response with `data: null` and an issue in `errors`. The
issue has `code: "runtime_unavailable"`, a reason, a `retryable` flag, and structured `details`. Follow that flag and
any recovery command; not every runtime refusal is retryable. See the [response schema](tool-catalog.md#response-shape)
for the full shape.

| Failure | Where it appears |
| --- | --- |
| Lean parse/elaboration diagnostics, kernel rejection, meta timeout | `data`; a failed proof is a completed tool call. |
| Admission timeout, worker death, or session loss | `errors` with `code: "runtime_unavailable"`; retryability depends on the outcome. |
| Heap-ceiling overrun | May abort the child and enter worker-death recovery; do not assume a Lean diagnostic. |
| I/O/config errors, internal-invariant violations, unusable Lake project | MCP error. |

Warnings also appear in `errors`, with `severity: "warning"` and an optional `next_action`. A completed fresh retry can
return usable data alongside a warning about its earlier failed attempt.

### Internal runtime facts

The operation layer still computes freshness/import and runtime facts before semantic response adaptation. Runtime facts
include worker generation, whether a call observed or performed a restart, retry count, controller queue wait, RSS when
available, import profile, profile-switch count, and restart history. These remain telemetry and are omitted at the
default quiet verbosity. Proof-relevant artifact facts are public under `trust.artifacts` and survive the quiet
telemetry gate: source snapshots (`source` / `file` / `edit_fresh`), build artifacts (`olean` or `ilean` with
`build_fresh`, `stale_build`, or `missing_build`), and worker/toolchain availability facts. Each source fact includes
`content_sha256`, the digest of the exact bytes elaborated for that call.

## Capability shims and module queries

`lean_context(kind = "proof_position")` returns compact diagnostics, goals, and locals, with opt-in expected type and
proof boundaries. It depends on the optional bounded `lean_rs_host_process_module_query_batch` shim; a worker lacking
that capability answers `{ "status": "unsupported" }`. No public tool requests or caches whole-file info trees. With
full telemetry, `query_facts` reports worker cache status, output bytes, and phase timings. Repeated calls reach the
worker snapshot cache, so warm behavior is observable.

Files whose headers import modules absent from the open environment are still processed; missing imports produce a
semantic warning issue. Files using Lean 4's module-system header syntax — `module`, `public import`, `import all`, and
`meta import` — are supported. A header that doesn't parse short-circuits to `header_parse_failed`.

Unlike an external LSP process, the host can still start when unrelated project modules are broken. Calls whose imports
avoid the broken module continue to work; a broken target file reports structured Lean diagnostics instead of a
bootstrap failure.

## Installing workers

The target Lake project's requested imports must have built `.olean` files. Any ordinary Lake project can be served; it
need not copy the [test fixture](../fixtures/lean/README.md), declare host shims, or build its own shared facet for the
server. The worker uses the shims bundled with `lean-rs-host`.

`install-worker` compiles a worker for each Lean toolchain. Its `build.rs` embeds an absolute rpath to that toolchain's
`lib/lean`. Source selection is automatic:

- **Registry** (the default for a `cargo install lean-host-mcp` binary): fetch and build the published
  `lean-host-mcp-worker` crate at the server's own version (`cargo install lean-host-mcp-worker --version =<ver>`).
- **Local workspace** (automatic when running from a checkout): build the worker from the workspace source
  (`cargo build -p lean-host-mcp-worker`), reusing cargo's incremental cache.

The detection is "was this binary built from a checkout that still has the worker crate beside it?" — no flag needed.
`--source-dir <path>` overrides it to build from an explicit checkout (useful if the original checkout moved after the
binary was built). Either way the worker needs a Rust toolchain on `PATH` and the matching Lean toolchain installed via
elan; the freshly built worker is smoke-tested before it is recorded as usable.

### Keeping workers compatible with the host

The provenance sidecar records the worker wire-protocol version independently of the package version:

- `install-worker --auto` (the default) scans `~/.elan/toolchains` and (re)builds any worker that is **missing or
  stale** — protocol mismatch, `lean.h` header drift, or a failed/absent runtime smoke record — and skips ones that are
  current. Out-of-window toolchains are skipped (a worker for them could never load). `--force` rebuilds current workers
  too (e.g. to re-run the smoke test or replace a corrupted binary).
- `install-worker --toolchain <id>` builds a fresh generation in its final directory. The installer verifies the
  published SDK shim sources, builds them with the standalone SDK writer, and records their identities in
  `worker.json`. Use `--shim-source DIR` or `--standalone-writer PATH` for explicit inputs. After the builtin
  `Nat.add_zero` inspection and successful shutdown it atomically updates `current-generation`. Existing workers
  retain their original paths. Installs without a selector continue to use the legacy directory.
  The installed shims and their compiled Lean imports run without the source checkout or Cargo source cache.
  To use a different worker root, reinstall there. The SDK manifest records absolute paths for its generation.
- `install-worker --list` prints every installed worker; the `protocol` column reads `compatible`, `incompatible`, or
  `unknown` for a legacy sidecar. The runtime handshake remains authoritative for legacy workers.
- `install-worker --clean` removes all installed workers; `--clean --toolchain <id>` removes just one. Workers are
  rebuildable artifacts, so this only deletes from the install root and never touches source. Use it for disk hygiene or
  to force a clean rebuild after a `lean-rs` ABI change.
- `install-worker --prune` removes only *unservable* workers — those outside the supported window or with a recorded
  smoke-test failure. Other stale workers are kept; rebuild those with `--auto`.

At runtime, package-version skew is silent when the protocol matches. A recorded protocol mismatch is a non-retryable
`runtime_unavailable` response with `lean-host-mcp install-worker --auto` as the exact recovery command. Header drift or
smoke failure is also a hard refusal with an actionable reinstall command.

### Managed source-bound writer cutover

Source-bound staging is opt-in. After draining the supported launchers, install the matched digest-aware host and
standalone SDK writer. Use that writer's `writer-storage approve-handoff` with the existing registry path, device,
and inode. Set `LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER` on the managed broker to the approved JSON fields
`ownership_registry`, `registry_device`, `registry_inode`, `executable`, `sha256`, and the returned
`approval_generation`. The broker reports that approval in its compatibility receipt. The SDK enforces the
generation at admission. Startup does not issue approval.

For managed rollback, disable source-bound admission and pause the supported launchers. Use the same pinned
SDK writer's `writer-storage revoke-handoff` on the original registry. If revocation is busy or its result is
unknown, keep the launchers paused and retry after custody closes. Once revocation succeeds, restart the matched
digest-aware host with `LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER` unset. Keep the same registry and lock inodes.
Use `make mcp-rollback` for the supported Proofs launcher. This procedure has been qualified with source-built
images. Published package identities require a repeat with the installed artifacts.

### Enrolled warm launcher transition

After draining the selected project, use the installed host built with `managed-warm-inspections` and the original
custody descriptor to switch an enrolled ordinary project to warm managed serving.

```sh
lean-host-mcp launcher-transition --project "$PROJECT" --from-generation "$GENERATION" \
  --to managed --promote-warm-store --custody "$CUSTODY"
```

The transition durably promotes the original store before publishing the managed selector. If interrupted, retry the
same command and retain the original store and custody descriptor. An already promoted store keeps its UUID and
results. Start the matching managed host with `--managed-warm-inspections` after the selector transition succeeds.
Cold transitions omit `--promote-warm-store`. For ordinary rollback, drain both broker and callers, verify the store
has settled, and switch with `--to ordinary` on the same matching host. The store schema remains at its promoted version.

A warm attempt can report `confirmed_ended` while its generation still holds custody for reuse. After a drain,
an original-key GET that verifies exact generation retirement also settles that generation's eligible ended
attempts in the store. This preserves saved result identities and applies normal acknowledged-result compaction.
The pool's stopped status alone does not reconcile the durable stage rows.

## Build, test, lint

```sh
cargo build --locked -p lean-host-mcp                          # parent only
cargo build --locked -p lean-host-mcp-worker                   # worker only (links libleanshared)
cargo clippy --locked --workspace --all-targets -- -D warnings # safe; clippy doesn't link
cargo test --locked -p lean-host-mcp --lib                      # parent unit tests
cargo test --locked -p lean-host-mcp-worker --all-targets       # worker tests
scripts/test-installed-stdio.sh --all-targets                  # includes installed-worker EOF and exit
scripts/test-installed-module-setup.sh                         # packaged proof and semantic search
scripts/check-parent-link.sh                                  # build and inspect the release parent
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
    cargo test --locked -p lean-host-mcp --test e2e -- --ignored
```

Build each member with `-p <name>`. Selecting both in `cargo build --workspace` unifies the `lean-rs-sys` feature set
and silently links `libleanshared` into the parent. Run the packaged smoke test from a clean checkout. It uses
`cargo package --workspace --locked` to verify the archives and installs from the unpacked sources in the target
directory's `package/` subdirectory.

The installed smoke scripts require Python 3.11 or later and a native build. They resolve the pin in
`fixtures/lean/lean-toolchain` through Elan and use Cargo's configured target directory for artifacts. Keep the matching
toolchain under `~/.elan/toolchains` for worker installation and discovery. `LEAN_SYSROOT` can select a matching prefix
for the build. Run `actionlint` and `shellcheck scripts/*.sh` to lint the workflow and shell files.

Dispatch the Release workflow from `main` to rehearse verification and packaging. A push of a matching `v<version>` tag
triggers publication after the changelog and packages pass verification. To recover after partial publication, rerun
the original tagged workflow. It skips crate versions already present on crates.io and continues to GitHub Release creation.

## Smoke and performance baseline

The ignored `smoke_perf` integration test is the black-box baseline harness for proof-agent work. It starts the compiled
stdio MCP server, calls `tools/list`, runs representative tool calls, and emits JSONL rows with wall time, serialized
response bytes, 32 KiB / 64 KiB budget flags, status, warning count, observable project-session changes, and process RSS
when the platform exposes it. For `lean_context(kind = "proof_position")`, rows also include the worker module-cache
status, worker-reported output bytes, phase timings, and optional worker cache size facts. The budget constants are
test-only guardrails: ordinary model-facing responses should aim for 16–32 KiB, with 64 KiB as the default hard ceiling.
Production truncation is still tool-specific policy.

```sh
cargo build -p lean-host-mcp
cargo test -p lean-host-mcp --test smoke_perf -- --ignored --nocapture

LEAN_HOST_MCP_SMOKE_PROJECT=/path/to/your/lake/project \
  LEAN_HOST_MCP_SMOKE_FILE=Relative/Module/File.lean \
  LEAN_HOST_MCP_SMOKE_DECLARATION=Your.Namespace.declaration \
  cargo test -p lean-host-mcp --test smoke_perf -- --ignored --nocapture
```

Keep the JSONL output with performance changes so comparisons use the same workload, byte accounting, and cold/warm
worker behavior.

### Memory stability

`scripts/memory_stability.py` answers the orthogonal question: does a long-lived server grow with *call count*? It
replays a workload JSON `--repeats` times against one server and reports `peak_import_residue_mib`,
`final_import_residue_mib`, `final_worker_generation`, and `call_restart_count` per run. Residue is monotone within a
generation, so `peak == final` with the generation unmoved is the flat-growth result.

```sh
uv run scripts/memory_stability.py \
  --project-root /path/to/lake/project \
  --workload scripts/memory_stability.kanproofs.json \
  --server-bin target/release/lean-host-mcp \
  --workers-dir target/release \
  --repeats 15
```

Measured on a Mathlib-scale project (kan-proofs, four tools against one file, 15 repeats — 60 calls): **60/60 `ok`,
generation 1 throughout, 0 restarts, residue 1786 MiB peak and final against a 9216 MiB budget, RSS 5.46 GB.** This
baseline exercises a repeated import profile; it does not predict the residue growth of a changed-file sweep. The latter
is bounded by the residue budget and covered by `crates/lean-host-mcp/tests/kanproofs_eval.rs`.

Native worker admission requires an operator-selected primary project. Set `--lake-root`, `LEAN_HOST_MCP_PROJECT`, or an absolute `primary_project` in the home configuration. CLI and environment selection take precedence over the home setting. Filesystem discovery and project-local configuration can select a project for metadata. A native request using discovery or an explicit `project=` must resolve to the approved canonical directory before the broker prepares Lake options or opens a worker. Worker-side source and capability checks still apply.

To verify a file in an already-built project, start the stdio server with `lean-host-mcp --lake-root /absolute/path/to/project`. Call `lean_verify` with a project-relative file and a declaration, for example:

```json
{"targets":[{"kind":"explicit","file":"Demo.lean","declarations":["Demo.exampleTheorem"]}]}
```

The server runs the selected toolchain's `lake --no-build --no-cache setup-file <file>` under the project writer and checks the current source using that file's options, imports, plugins and dynamic libraries. Keep the project's existing compiled artifacts available. Missing setup inputs produce an error. This route requires the setup-aware verification shim and takes no `--prepared-generation` argument.

To charge ordinary file setup, supply both `--module-setup-writer PATH` and `--module-setup-storage-request JSON`
at server startup for the operator-approved primary project. Use the matched standalone writer and its v1 full-turn
storage request, including a positive byte budget, free-space floor, and directory identities for the project,
registry, scratch parent, inherited temporary directory and any additional participating roots. Lake uses writer
scratch for its artifact cache. A refused charge or changed writer fails the call. Omitting both options keeps ordinary setup.

Use `--prepared-generation` only for the separately prepared worker-import path. Ordinary file-backed `lean_lookup` with `kind="proof_search"` uses the selected file’s Lake setup for goal elaboration.

The optional CLI-only `--verification-approval verify-batch-v1:<sha256>` records one verification policy digest for the operator-selected canonical project at startup. The digest binds independently reviewed source and batch options, the installed generation and its ordered import domains. There is no project-local or per-request setter. The startup binding alone grants no native source operation.

**Elan maintenance.** Keep the selected canonical Elan toolchain installed and unchanged while its workers run. Drain and stop those workers before uninstalling or replacing that toolchain. The host checks release identity at admission; it does not acquire an Elan maintenance lock. Generation-based installs retain shared artifact locks. Clean and prune require exclusive access to those same domains, including older generations in the slot. Flat legacy installations predate these locks and require their workers to be stopped before maintenance.
