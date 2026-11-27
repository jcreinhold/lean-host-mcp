# Operations

Configure and troubleshoot the server here. Start with the [README](../README.md) for installation and client setup; use
the [tool catalog](tool-catalog.md) for request and response contracts.

- [Configuration](#configuration-file) and [reference table](#configuration-reference)
- [Artifact freshness](#rebuilt-artifacts) and [memory sizing](#memory-and-import-cycling)
- [Builds](#builds) and [concurrency and admission](#concurrency-and-admission)
- [Logging](#request-activity-in-the-terminal) and [worker recycles](#observing-worker-recycles)
- [Process lifetime](#process-lifetime) and [runtime failures](#runtime-error-contract)
- [Server installation](#installing-the-server), [worker installation](#installing-workers) and
  [test harnesses](#build-test-lint)


Worker lifeline sockets use `LEAN_HOST_MCP_SOCKET_DIR`, then `$XDG_RUNTIME_DIR/lean-host-mcp` when XDG is set, otherwise
`$HOME/.lean-host-mcp-ipc`. Choose a short canonical directory owned by the server user with mode 0700. On macOS a
socket path holds at most 103 bytes, so the directory's path may hold at most 54. The SDK removes each worker's socket
on closure. `serve` checks the directory when it starts and exits if it is unsafe or too long.

The server keeps its other state in its [store](#one-server-per-store), not under `HOME`: the SDK's durable owner
records in `owners/`, the semantic runtime cache in `semantic-runtimes/`, and in `processes/` the PID records that
`doctor processes` reads. `LEAN_RS_WORKER_OWNERS_DIRECTORY`, `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` and
`LEAN_HOST_MCP_PROCESS_REGISTRY_DIR` move each one. The server binds the owner directory before any native work, the SDK
creates it, and the server passes it to its SDK helpers as `LEAN_RS_WORKER_OWNERS_DIRECTORY`. Keep the same canonical
owner directory when restarting the server, including across binary changes. `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` must be
a canonical absolute path; the server creates the directory if it is missing. A server without `--lake-root` has no
store and fails every project call; its PID records go under the per-user cache directory.

The server finds the Lean toolchain under `ELAN_HOME`, else under the elan home that install recorded as
`[install] elan_home`, else under `$HOME/.elan`. It passes that home to Lake, Cargo and its helpers as `ELAN_HOME`.

The SDK writer executable setting is `LEAN_RS_WRITER_EXECUTABLE`. The server still selects its embedded writer for
ordinary operations and honors explicit standalone-writer selections.

## Configuration file

The server reads two TOML files at its `--lake-root`, and no others:

- `lean-host-mcp.toml`, tracked with the project;
- `.local/lean-host-mcp/config.toml`, which [`lean-host-mcp install`](#installing-the-server) writes. Its directory
  holds a `.gitignore` that ignores everything in it.

Keys in the local file win over the tracked file one key at a time, so each file need only set what it changes. The
server does not look in its working directory, its parent directories or your home directory. Without `--lake-root` it
reads no file. Missing files are ignored; parse failures are logged and skipped. A merged value with the wrong type
causes the merged file configuration to be ignored. Tables the server does not know, such as an old `[launcher]`, are
ignored.

Generate a commented tracked file with the current defaults:

```sh
lean-host-mcp config init                       # ./lean-host-mcp.toml
lean-host-mcp config init --lake-root PROJECT   # PROJECT/lean-host-mcp.toml
```

`--path FILE` selects another destination. Existing files require `--force` to overwrite.

## Configuration reference

For each setting, precedence is **CLI flag > environment variable > local file > tracked file > built-in default**.
Units are shown in the table; GiB figures in descriptions are explanatory, not literal setting values.

<!-- BEGIN GENERATED: do not edit by hand. Regenerate from `config_schema::render_reference_table`; the `operations_md_reference_table_is_in_sync` test fails when this block drifts. -->
| Key | Type | Default | Override | Description |
| --- | --- | --- | --- | --- |
| `runtime.lean_max_memory_kib` | integer (KiB) | unset | `LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB` | Lean heap ceiling per worker child. Allocator exhaustion can abort the child when a C++ exception reaches the Rust FFI boundary. Unset derives the residue budget plus max(residue budget, 8 GiB), converted to KiB, leaving headroom above clean import recycling. Startup warns when an explicit ceiling is at or below the residue budget. |
| `runtime.request_timeout_millis` | integer (ms) | `120000` | `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS` | One shared budget for worker-capacity admission, mailbox admission and queued waiting for each worker operation. After dispatch, worker execution has a separate timeout of the same length; execution expiry recycles the worker. Admission expiry does not dispatch that queued operation. This is not a whole-tool deadline: preparation and multiple operations can take longer. Default 120 s. |
| `runtime.project_mailbox_capacity` | integer | `16` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | Maximum queued calls in each worker actor's mailbox. Further callers wait in FIFO order for mailbox space within the same original request_timeout_millis admission deadline. Cancellation removes only that caller. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many worker restarts are tolerated within the restart window before the project is marked unhealthy. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `runtime.worker_import_residue_budget_mib` | integer (MiB) | unset | `LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB` | Unreclaimable import bytes per worker before a clean recycle. Unset uses a quarter of RAM divided among broker.max_workers, clamped to 9-12 GiB and capped at each worker's RAM share minus 4608 MiB (minimum 1 MiB). This affordability cap can lower the default below 9 GiB. Explicit MiB values bypass RAM sizing; a budget below one import's residue can force frequent recycling. |
| `runtime.worker_session_pool_capacity` | integer | `8` | `LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY` | How many imported Lean environments one worker child keeps warm. Returning to a pooled import profile costs a key comparison instead of a full import, so this should cover the number of distinct per-file import profiles a session moves between — a changed-file sweep touches one per file. A held environment costs about 70 MiB against 2-4 GB for the import it saves. Default 8. |
| `broker.max_workers` | integer | `4` | `LEAN_HOST_MCP_MAX_WORKERS` | Worker processes the server may run at once, counting workers that are starting or still exiting. An exiting worker keeps its slot until its exit is confirmed, so a value above max_workers_per_project lets a new worker start while an old one exits. Default 4. |
| `broker.max_workers_per_project` | integer | `1` | `LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT` | Workers that may serve the project at once. An extra worker opens only when a slot is free; otherwise requests queue on an existing worker. Set 2 only after measuring the memory cost of concurrent checks: each worker keeps its own imports and cache. Default 1. |
| `broker.idle_timeout_secs` | integer (s) | `600` | `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Stop a worker after this many idle seconds. 0 turns idle retirement off. Default 10 minutes. |
| `server.bind` | string (loopback ADDR:PORT) | unset | `--bind / LEAN_HOST_MCP_BIND` | Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS. |
| `server.http_path` | string | unset | `--http-path / LEAN_HOST_MCP_HTTP_PATH` | HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp. |
| `server.response_carrier` | string (text, structured, both) | `"text"` | `LEAN_HOST_MCP_RESPONSE_CARRIER` | Which field of the tool result carries the semantic response. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text. |
| `telemetry.verbosity` | string (quiet, full) | `"quiet"` | `LEAN_HOST_MCP_TELEMETRY_VERBOSITY` | How much operational telemetry the internal operation envelope keeps before semantic response adaptation. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet. |
| `output.max_field_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES` | Override the per-field output byte cap for all tools. Unset keeps each tool's built-in default (8 KiB for inspection, 4 KiB for proof actions). Clamped to 256 bytes to 64 KiB. |
| `output.max_total_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES` | Override the total output byte cap for all tools. Unset keeps the built-in 64 KiB default. Clamped to 1 KiB to 64 KiB. |
| `output.heartbeat_limit` | integer (heartbeats) | unset | `LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT` | Default elaboration heartbeat budget for lean_trial proof_step and lean_verify target groups. Unset uses the worker default. Bounds runaway tactics. |
| `output.slow_call_warning_millis` | integer (ms) | `10000` | `LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS` | Worker-call CPU time above which every tool response carries a warning naming the elapsed milliseconds and recommending refactoring (splitting the declaration, explicit type annotations, extracted lemmas). Keys on call_cpu_millis, not wallclock, so machine contention never triggers it. Default 10 s. |
| `install.binary` | path | unset | none | The server executable that install copied, for MCP client configs and scripts that run the server by absolute path. The server itself ignores it. |
| `install.workers_dir` | path | unset | `LEAN_HOST_MCP_WORKERS_DIR` | Directory of per-toolchain worker installs, shared by install, install-worker and the server. Unset uses .local/lean-host-mcp/workers under the Lake root. A relative path is relative to the Lake root. |
| `install.store_dir` | path | unset | `LEAN_HOST_MCP_STORE_DIR` | Private server state directory (mode 0700). Its lock admits one server at a time; a second server on the same store exits and names the first. It also holds the native owner records (owners/), the semantic runtime cache (semantic-runtimes/) and the server process records (processes/). Unset uses .local/lean-host-mcp/store under the Lake root. A relative path is relative to the Lake root. |
| `install.elan_home` | path | unset | `ELAN_HOME` | Elan's home when install ran: ELAN_HOME, else $HOME/.elan. When ELAN_HOME is unset, the server and install-worker --lake-root look for the Lean toolchain here, not under $HOME, and pass it on as ELAN_HOME. |
| `requests.retention_hours` | integer (h) | `168` | none | Hours an ended request keeps its row, its stored result and its build log. The server prunes at start and every hour; it never prunes a request that has not ended. Default 7 days. |
| `requests.max_rows` | integer | `10000` | none | Most rows the request table keeps. Past this, pruning deletes the ended rows that finished first. Default 10000. |
| `requests.wait_max_secs` | integer (s) | `300` | none | Longest wait_seconds one lean_get may ask for. Default 300 s. |
| `build.guard` | array of Lake targets | `[]` | none | Targets that must already be built. A guard step runs lake build --no-build on them, which fails without building anything when one is out of date. It runs before a build of targets; a recipe's guard key places it. Proofs uses ["@mathlib/+Mathlib:olean"]. Empty runs no guard. |
| `build.timeout_secs` | integer (s) | `14400` | none | Deadline for one build, counted from its request: the wait for the project's write turn and every step count. Default 4 hours. |
| `build.log_limit_mib` | integer (MiB) | `16` | none | Cap on one build's log, which lives in the store's logs directory. Output past the cap is dropped, and the reply says so. Default 16 MiB. |
| `recipes.<name>.steps` | array of arrays of strings | unset | none | The Lake runs of one recipe, in order. Each is the words after lake and may not be empty; Lake reports its own errors in them. The first run that fails ends the recipe. Steps run in the Lake root with the server's environment. lean_build runs a recipe by its name. |
| `recipes.<name>.append` | string (none, last, each) | `"none"` | none | Which steps get the request's args at their end: none refuses args, last gives them to the last step, each to every step. An arg may not be empty or start with -. Default none. |
| `recipes.<name>.guard` | string (before, after, none) | `"before"` | none | Where the build.guard step runs: before the steps, after them, or not at all. A failing guard before the steps ends the recipe. Default before. |
| `recipes.<name>.timeout_secs` | integer (s) | unset | none | Deadline for one build of this recipe, in place of build.timeout_secs. Unset uses build.timeout_secs. |
<!-- END GENERATED -->

`output.max_field_bytes` and `output.max_total_bytes` bound model-facing payloads before they leave the worker. Tight
caps are useful for smoke tests and for very large proof states, but they can also make proof-step batches partial: once
the total proof-action budget is exhausted the current candidate reports `budget_exceeded` and later candidates report
`not_attempted`. Retry the promising snippet alone, or raise `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES`, when the response
summary shows nonzero `budget_exceeded`, `not_attempted`, or `output_truncated` counts.

### Rebuilt artifacts

A worker session's Lean environment is a snapshot taken at import, and a check reads `.olean` files while it runs, so no
check may overlap a build of the same project. The server keeps them apart with one fair turn per Lake root:

- A check takes a read turn before it starts a worker job or a native reader, then the shared kernel lock on the
  project's artifacts, and keeps both until it ends. Read turns do not exclude each other.
- `lean_build` takes the write turn, stops the project's workers and waits until each has exited, then takes the
  exclusive kernel lock through the writer helper. It waits for the checks that hold read turns and holds back the
  checks that arrive after it, so a check sent during a build waits and then sees what the build wrote. The next check
  starts a fresh worker, even after a build that changed nothing. Turns follow arrival order; a waiting request keeps
  its place until its deadline.
- A worker holds the shared lock for its whole life. When a writer in another process asks for the exclusive lock, the
  server queues a write turn for it and retires the idle workers that hold the lock.

Each read turn carries the number of builds that have finished on the root. A worker that loaded its imports before a
later build finished is recycled before reuse, and `artifacts_rebuilt` records that. Since a build in this server stops
the workers first and workers hold the shared lock for their whole life, this is a backstop for writers in other
processes. A cached query result answers only while the worker that computed it lives.

Build through `lean_build`, or through another process that holds the shared writer. A raw `lake build` takes no lock:
it can rewrite `.olean` files while a check reads them, and the server does not notice. That is outside the contract.
Bundled shims and installed toolchain contents remain trusted fixed runtime inputs.

File checks and `lake setup-file` runs are native readers. The SDK starts one at a time per owner registry, across all
projects and servers that share it. A reader that cannot start within 30 seconds, or by its request's deadline if that
comes first, fails. Each start first removes the files of readers that have ended, while an ended reader may still be
removing its own inputs. When a start finds such a file gone, it logs `native reader admission found a file gone` and
tries again, up to three times. Every file check also takes shared custody of the installed shims. When checks start
together, one may find the SDK's admission gate on the shims held for a moment; it tries again every 20 ms until its
request's time runs out.

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
9 GiB.

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

## Builds

`lean_build` runs Lake in the Lake root and replies when Lake is done. A request names Lake `targets` (none: Lake's
default targets) or a `recipe` from the tracked config, plus `args` where the recipe's `append` lets them in. For
example:

```toml
[build]
guard = ["@mathlib/+Mathlib:olean"] # must already be built; checked with lake build --no-build
timeout_secs = 14400                # from the request: the wait for the turn and every step count
log_limit_mib = 16

[recipes.clean]
steps = [["clean", "Proofs"]]
guard = "none"

[recipes.format]
steps = [["build"], ["exe", "lean-fmt", "format", "--check", "--no-build"]]
append = "each"                     # the request's args go at the end of both steps
```

A build takes the project's write turn (see [rebuilt artifacts](#rebuilt-artifacts)), stops the project's workers, and
opens one writer helper, which holds the exclusive kernel lock while the guard and each step run in it. The first
failing step ends the build. All output goes to one log under the store's `logs/`, capped at `build.log_limit_mib`; the
reply gives each step's command and exit code, the log's path and its tail. At the deadline the server stops the running
step. The turn ends only after the helper and every process of the build have exited, so no check starts while one still
runs.

Each step runs in the Lake root with the server's environment, except that the server sets `LEAN_SYSROOT` to the
project's toolchain and sets `LAKE_ARTIFACT_CACHE=false` so that builds land in each package's `.lake/build`. Set a
variable such as `LEAN_NUM_THREADS` where the server starts. The server refuses to start with `LEAN_PATH` set: each
check takes its search path from the Lake manifest, and workers refuse an inherited one. The server checks only the
words a client adds: a target or recipe argument may not be empty or start with `-`, so a client cannot pass Lake an
option such as `-d`. Recipe words come from the tracked config, and Lake reports its own errors in them. Lake decides
what to build and in which order.

`lean-host-mcp config check --lake-root PROJECT` reads both config files, names every unknown key and bad value, and
prints `[build]` and each recipe with their defaults. It runs no Lake and writes nothing, so it is safe beside a running
server.

`lean_imports` lists, for each file, its whole transitive import closure as `lake setup-file --no-build` reports it
(`importArts`), not only its direct imports: each module, its `.olean`, and its source when Lake built it under the
project's `.lake/build`. It builds nothing: a missing or stale import fails the call with `setup_file_failed`, naming
the file and quoting Lake.

## Concurrency and admission

A server serves one Lake project, its `--lake-root`, and refuses calls on any other. Each worker owns a child process
and a dedicated thread that runs one job at a time. `broker.max_workers_per_project`, default one, caps the workers that
serve the project at once. `broker.max_workers` caps worker slots, counting workers that are starting or still exiting.
With a larger per-project limit, the broker prefers an idle worker, starts another only when a slot is free, or queues
on the least-loaded existing worker. Each worker keeps its own import and result caches.
`runtime.project_mailbox_capacity` bounds the actor's mailbox. Further callers wait for mailbox admission in FIFO order
rather than retrying `mailbox_full`. A cold start waits in FIFO order for a slot that an exiting worker still holds.
Cancellation removes only its own waiter, and a slot stays reserved until its worker's exit is confirmed.

Worker-capacity admission, mailbox admission, and time waiting in the actor queue share one
`runtime.request_timeout_millis` deadline. Expiry returns `project_admission_timeout`, `mailbox_admission_timeout`, or
`queue_wait_timeout` without dispatching that queued operation. Once dispatch begins, the worker execution timeout
applies separately. This is not a whole-tool deadline: a tool that submits several worker operations can take longer. A
named check is the exception: its row fixes one deadline of the same length for the whole call, counted from the moment
the server records it. Parallel server processes remain independent; lower `broker.max_workers` when the machine cannot
hold that many workers.

Cheap metadata paths never open a worker at all: degraded `needs_build` responses, invalid-request responses, and
project-scope `.ilean` reference reads answer from the Lake files directly. A repeat module query on an unmodified file
is answered from the per-project result cache and likewise never enters the mailbox, so a warm call does not queue
behind in-flight Lean work.

Semantic proof ranking briefly opens a companion child in addition to the elaboration worker. Include that peak in
memory planning. The server shuts it down after the call; it does not keep a separate resident ranking pool.
Measurements in `project.rs::open_semantic_capability` found no significant improvement from keeping it resident.

### Request table

A server started with `--lake-root` keeps one row per [named request](tool-catalog.md#named-requests) in
`requests.sqlite3` in its store, and a build's log in the store's `logs/`. The table uses SQLite's write-ahead log with
`synchronous=FULL` (and `fullfsync` on macOS), so a row the server has written survives a crash of the server or the
machine. The file has mode 0600. A server without a store has no table and refuses request ids with
`requests_unavailable`.

The server prunes the table when it starts and every hour after. It deletes ended rows older than
`requests.retention_hours`, then the oldest ended rows past `requests.max_rows`, and then their logs. It never prunes a
row that has not ended. At start it also deletes every log that no row names.

Before it listens, a starting server ends the rows an earlier server left. For each project with a `running` row it
takes the write turn and waits, for up to 15 minutes, until the writer lock is free, which shows that no helper of the
earlier server still runs; the rows then become `interrupted`. If the lock stays held, the server does not start. It
then starts the `queued` rows in the order they came, or ends them as `timed_out` past their deadline and as
`interrupted` when their project is gone. No row that said `running` runs again.

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

Direct calls and named requests share this boundary. A retry with the same `request_id` or a `lean_get` does not log a
second execution. Routine `lean_runtime` status and `lean_get` polling remain quiet. If a future disappears before it
returns, a warning explicitly says worker completion is not confirmed; that log is not a receipt proving that an
executing worker stopped.

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
- `info` — `artifacts_rebuilt` (a build finished after the child loaded its imports, so it was recycled; a backstop, see
  [rebuilt artifacts](#rebuilt-artifacts)), `import_residue` (the reactive half of the residue budget — the cycle a call
  had to wait for, and the frequency an operator tunes against), memory-pressure cycles (`rss_post_job`), plus
  `opened project` / `idle reaper evicted projects` lifecycle lines.
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
outcome. Abrupt parent death can still skip Rust `Drop`; child-side parent-loss handling is best effort. Stronger
containment is the job of the process manager that starts the server.

On shutdown the server stops its named requests and leaves their rows as they are. At the next start a `running` row
becomes `interrupted` and a `queued` row runs (see [request table](#request-table)). A build's writer helper ends when
the server dies, however it dies, so a killed server's build does not finish.

Every running server writes a PID record in its store's `processes/` and removes it on normal shutdown. A server without
`--lake-root` writes it under the per-user cache directory at `lean-host-mcp/processes/` instead, and
`LEAN_HOST_MCP_PROCESS_REGISTRY_DIR` overrides both. The record contains the exact server PID, executable path, working
directory, transport, bind/path, startup parent PID, and process group. Inspect the records of a project's servers with
the first command, and those of servers without a Lake root with the second:

```sh
lean-host-mcp doctor processes --lake-root PROJECT
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

### One server per store

A server started with `--lake-root` takes an exclusive `flock` on `server.lock` in its store and writes its pid and Lake
root into that file. The kernel drops the lock when the process exits, however it exits. A second server on the same
store exits at startup and names the first:

```text
another lean-host-mcp (pid 4242, lake root /work/project) holds the store /work/project/.local/lean-host-mcp/store; stop it first
```

The store is `LEAN_HOST_MCP_STORE_DIR`, else `[install] store_dir` from the config files, else
`.local/lean-host-mcp/store` under the Lake root. The server creates it with mode 0700 after its config checks pass. A
server without `--lake-root` has no store and takes no lock.

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

## Installing the server

`lean-host-mcp install --lake-root PROJECT` makes one Lake project ready to serve. It reads the toolchain from
`PROJECT/lean-toolchain` and writes only under `PROJECT/.local/lean-host-mcp/`, never a tracked file:

| Path | Contents |
| --- | --- |
| `.gitignore` | `*`, so Git ignores the directory even when the project does not ignore `.local/` |
| `bin/lean-host-mcp` | a copy of the running executable |
| `bin/lean-rs-writer` | the SDK writer that builds and checks the worker's shims |
| `workers/` | the worker for the project's toolchain, built as `install-worker` builds it |
| `store/` | the server's private state, mode 0700, including the native owner records that install binds |
| `config.toml` | the local config file, with `[install]` set to the absolute `binary`, `workers_dir`, `store_dir` and `elan_home` |

Serve with the installed copy:

```sh
PROJECT/.local/lean-host-mcp/bin/lean-host-mcp --lake-root PROJECT
```

- `--dir DIR` puts `bin/`, `workers/` and `store/` under `DIR`. The config file stays at
  `PROJECT/.local/lean-host-mcp/config.toml`, where the server reads it.
- Without `--dir`, the worker and store directories follow the server's order: `LEAN_HOST_MCP_WORKERS_DIR` or
  `LEAN_HOST_MCP_STORE_DIR`, then an `[install]` value already in the config files, then the default above.
- A current worker or writer stays; `--force` replaces both.
- `--standalone-writer PATH` installs that writer. Otherwise install keeps a current `bin/lean-rs-writer`, copies a
  current one from beside the running executable, or runs `cargo install lean-rs-writer` at the pinned SDK version.
- `--shim-source DIR` and `--source-dir DIR` pass to the worker build, as for `install-worker`.

Install replaces the `[install]` table and keeps the local file's other keys, but not its comments. It refuses a local
file that is not valid TOML before it builds anything. It records `elan_home` as `ELAN_HOME`, else `$HOME/.elan`, so a
server started without `ELAN_HOME` finds the toolchain without reading `HOME`. Install, then serve, write nothing under
`HOME` but the socket directory; `install_and_serve_keep_no_state_in_an_empty_home` in `tests/http_transport.rs` checks
this.

## Installing workers

The target Lake project's requested imports must have built `.olean` files. Any ordinary Lake project can be served; it
need not copy the [test fixture](../fixtures/lean/README.md), declare host shims, or build its own shared facet for the
server. The worker uses the shims bundled with `lean-rs-host`.

`install-worker` compiles a worker for each Lean toolchain. Its `build.rs` embeds an absolute rpath to that toolchain's
`lib/lean`. It installs into `--workers-dir DIR`, or into the worker directory of `--lake-root PROJECT`:
`LEAN_HOST_MCP_WORKERS_DIR`, then `[install] workers_dir`, then `PROJECT/.local/lean-host-mcp/workers`. The server finds
workers through the same function. With neither flag, `install-worker` uses `LEAN_HOST_MCP_WORKERS_DIR` and fails when
it is unset. Source selection is automatic:

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

- `install-worker --auto` (the default) scans `toolchains/` in the elan home and (re)builds any worker that is **missing
  or stale** — protocol mismatch, `lean.h` header drift, or a failed/absent runtime smoke record — and skips ones that
  are current. Out-of-window toolchains are skipped (a worker for them could never load). `--force` rebuilds current
  workers too (e.g. to re-run the smoke test or replace a corrupted binary).
- `install-worker --toolchain <id>` builds a fresh generation in its final directory. The installer verifies the
  published SDK shim sources, builds them with the standalone SDK writer, and records their identities in `worker.json`.
  Use `--shim-source DIR` or `--standalone-writer PATH` for explicit inputs. After the builtin `Nat.add_zero` inspection
  and successful shutdown it atomically updates `current-generation`. Existing workers retain their original paths.
  Installs without a selector continue to use the legacy directory. The installed shims and their compiled Lean imports
  run without the source checkout or Cargo source cache. To use a different worker root, reinstall there. The SDK
  manifest records absolute paths for its generation.
- `install-worker --list` prints every installed worker; the `protocol` column reads `compatible`, `incompatible`, or
  `unknown` for a legacy sidecar. The runtime handshake remains authoritative for legacy workers.
- `install-worker --clean` removes all installed workers; `--clean --toolchain <id>` removes just one. Workers are
  rebuildable artifacts, so this only deletes from the install root and never touches source. Use it for disk hygiene or
  to force a clean rebuild after a `lean-rs` ABI change.
- `install-worker --prune` removes only *unservable* workers — those outside the supported window or with a recorded
  smoke-test failure. Other stale workers are kept; rebuild those with `--auto`.

At runtime, package-version skew is silent when the protocol matches. A recorded protocol mismatch is a non-retryable
`runtime_unavailable` response whose recovery command is `SERVER install --lake-root PROJECT`, where `SERVER` is the
running executable. A missing worker, header drift and smoke failure are also hard refusals. Each is one line that names
the worker directory and ends with the same command:

```text
no worker for leanprover/lean4:v4.36.0 in /work/project/.local/lean-host-mcp/workers; run: /work/project/.local/lean-host-mcp/bin/lean-host-mcp install --lake-root /work/project
```

### Source-bound writer cutover

Source-bound staging is opt-in. Stop the running servers, then install the matched digest-aware host and standalone SDK
writer. Use that writer's `writer-storage approve-handoff` with the existing registry path, device, and inode. Set
`LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER` on the server to the approved JSON fields `ownership_registry`,
`registry_device`, `registry_inode`, `executable`, `sha256`, and the returned `approval_generation`. The server reports
that approval in its compatibility receipt. The SDK enforces the generation at admission. Startup does not issue
approval.

To roll back, disable source-bound admission and stop the servers. Use the same pinned SDK writer's
`writer-storage revoke-handoff` on the original registry. If revocation is busy or its result is unknown, keep the
servers stopped and retry after custody closes. Once revocation succeeds, restart the matched digest-aware host with
`LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER` unset. Keep the same registry and lock inodes. This procedure has been qualified
with source-built images. Published package identities require a repeat with the installed artifacts.

## Build, test, lint

```sh
cargo build --locked -p lean-host-mcp                          # parent only
cargo build --locked -p lean-host-mcp-worker                   # worker only (links libleanshared)
cargo clippy --locked --workspace --all-targets -- -D warnings # safe; clippy doesn't link
cargo test --locked -p lean-host-mcp --lib                      # parent unit tests
cargo test --locked -p lean-host-mcp-worker --all-targets       # worker tests
scripts/test-installed-stdio.sh --all-targets                  # includes installed-worker EOF and exit
scripts/test-installed-module-setup.sh                         # packaged pair and semantic search
scripts/check-parent-link.sh                                  # build and inspect the release parent
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
    cargo test --locked -p lean-host-mcp --test e2e --test build_requests --test artifact_invalidation -- --ignored
LEAN_HOST_MCP_MATCHED_SDK_WRITER=/path/to/lean-rs-writer CARGO_NET_OFFLINE=true \
    cargo test --locked -p lean-host-mcp --test http_transport install_and_serve_keep_no_state_in_an_empty_home \
    -- --ignored                                               # install and serve with an empty HOME
```

On macOS, Cargo sets `DYLD_FALLBACK_LIBRARY_PATH` for the test binaries it runs, and the installed worker refuses any
`DYLD_*` variable, so opt-in tests that check files or start a worker fail under `cargo test`. Build them with
`cargo test --no-run`, then run each printed binary with `--ignored` under `env -u DYLD_FALLBACK_LIBRARY_PATH`. Point
`LEAN_HOST_MCP_WORKERS_DIR` at an installed worker, and keep `LEAN_RS_WORKER_OWNERS_DIRECTORY`,
`LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`, `LEAN_HOST_MCP_STORE_DIR` and `TMPDIR` under the target directory. The empty-HOME
test clears its children's environment, so it runs under `cargo test`. Build its writer from the lean-rs checkout at the
version `Cargo.lock` pins, with `--target-dir` under this repository's target directory.

Build each member with `-p <name>`. Selecting both in `cargo build --workspace` unifies the `lean-rs-sys` feature set
and silently links `libleanshared` into the parent. Run the packaged smoke test from a clean checkout. It uses
`cargo package --workspace --locked` to verify the archives and installs from the unpacked sources in the target
directory's `package/` subdirectory.

The installed smoke scripts require Python 3.11 or later and a native build. They resolve the pin in
`fixtures/lean/lean-toolchain` through Elan and use Cargo's configured target directory for artifacts. Keep the matching
toolchain in the elan home's `toolchains/` for worker installation and discovery. `LEAN_SYSROOT` can select a matching
prefix for the build. Run `actionlint` and `shellcheck scripts/*.sh` to lint the workflow and shell files.

Dispatch the Release workflow from `main` to rehearse verification and packaging. A push of a matching `v<version>` tag
triggers publication after the changelog and packages pass verification. To recover after partial publication, rerun the
original tagged workflow. It skips crate versions already present on crates.io and continues to GitHub Release creation.

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

A server serves the one project that `--lake-root` or `LEAN_HOST_MCP_PROJECT` names. An explicit `project=` must resolve
to that canonical directory before the broker reads the project or opens a worker; without a root every call fails.
Worker-side source and capability checks still apply.

To verify a file in an already-built project, start the stdio server with
`lean-host-mcp --lake-root /absolute/path/to/project`. Call `lean_verify` with a project-relative file and a
declaration, for example:

```json
{"targets":[{"kind":"explicit","file":"Demo.lean","declarations":["Demo.exampleTheorem"]}]}
```

Under the request's read turn, the server runs the selected toolchain's `lake --no-build --no-cache setup-file <file>`
as a native reader and checks the current source using that file's options, imports, plugins and dynamic libraries. Lake
builds nothing here, so keep the project built: when an import is missing or stale, Lake exits 3 and the reply is
`needs_build`, with a `stale_build` row naming each module Lake lists. This route requires the setup-aware verification
shim. File-backed `lean_lookup` with `kind="proof_search"` uses the same setup for goal elaboration.

Lake writes its own config cache under `.lake/config/` when the lakefile changed, under Lake's own lock, so a file check
right after a lakefile edit can write there. When two checks start that write at the same moment, Lake fails one with
"could not acquire an exclusive configuration lock"; retry it.

**Elan maintenance.** Keep the selected canonical Elan toolchain installed and unchanged while its workers run. Drain
and stop those workers before uninstalling or replacing that toolchain. The host checks release identity at admission;
it does not acquire an Elan maintenance lock. Generation-based installs retain shared artifact locks. Clean and prune
require exclusive access to those same domains, including older generations in the slot. Flat legacy installations
predate these locks and require their workers to be stopped before maintenance.
