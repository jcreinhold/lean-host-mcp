# Operations

How to run, upgrade and recover the server. The [README](../README.md) covers client setup, the
[tool catalog](tool-catalog.md) gives each tool's contract, and the [architecture](architecture.md) says why the server
works as it does.

## Run

### Install

`lean-host-mcp install --lake-root PROJECT` makes one Lake project ready to serve. It reads the toolchain from
`PROJECT/lean-toolchain` and writes only under `PROJECT/.local/lean-host-mcp/`, never a tracked file:

| Path | Contents |
| --- | --- |
| `.gitignore` | `*`, so Git ignores the directory even when the project does not ignore `.local/` |
| `bin/lean-host-mcp` | a copy of the running executable |
| `workers/` | the worker for the project's toolchain, in a new generation |
| `store/` | the server's private state, mode 0700, with the proof-search runtime in `semantic-runtimes/` |
| `config.toml` | the local config file, with `[install]` set to the absolute `binary`, `workers_dir`, `store_dir` and `elan_home` |

- `--dir DIR` puts `bin/`, `workers/` and `store/` under `DIR`; the config file stays where the server reads it.
- Without `--dir`, the worker and store directories follow the server's order: `LEAN_HOST_MCP_WORKERS_DIR` or
  `LEAN_HOST_MCP_STORE_DIR`, then an `[install]` value already in the config files, then the default above.
- A current worker stays, and `--force` replaces it. A worker that another release built is stale, so install replaces
  it after an upgrade.
- `--source-dir DIR` builds the worker from that checkout, as `install-worker` does.
- Install takes the store as a server does, so it refuses while a server serves the project.
- `[build] timeout_secs` limits the build of the proof-search runtime.

Install replaces the `[install]` table and keeps the local file's other keys, but not its comments. It refuses a local
file that is not valid TOML before it builds anything. It records `elan_home` as `ELAN_HOME`, else `$HOME/.elan`, so a
server started without `ELAN_HOME` finds the toolchain without reading `HOME`. Neither install nor the server writes
under `HOME`.

Any Lake project whose imports are built can be served; it needs no shims or extra targets. Install needs a Rust
toolchain on `PATH` and the project's Lean toolchain installed through elan.

### Serve

```sh
PROJECT/.local/lean-host-mcp/bin/lean-host-mcp --lake-root PROJECT                       # stdio
PROJECT/.local/lean-host-mcp/bin/lean-host-mcp --lake-root PROJECT --bind 127.0.0.1:8765  # Streamable HTTP at /mcp
```

A server serves the one project that `--lake-root` or `LEAN_HOST_MCP_PROJECT` names and refuses a call that names
another; without a root it fails every project call. It refuses to start with `LEAN_PATH` set, since each check takes
its search path from Lake and workers refuse an inherited one. It finds the Lean toolchain under `ELAN_HOME`, else under
the `[install] elan_home` that install recorded, else under `$HOME/.elan`, and passes that home on as `ELAN_HOME`.

A stdio server exits when the client closes its stdin, or when the process that launched it exits or changes, as the
kernel reports its parent PID. An HTTP server binds only a loopback address, since it has no authentication or TLS, and
exits on Ctrl-C or SIGTERM. Either way it closes its workers before it returns: queued calls fail with
`runtime_unavailable` and reason `project_shutting_down`, and a running call may finish or run to its timeout. It leaves
the rows of its named requests as they are, and the next server ends them
([named requests](architecture.md#named-requests)).

### The store

The store holds the server's state: `server.lock`, `children.lock` and `children/`, build logs in `logs/`, the inputs of
running short runs in `runs/`, the request table `requests.sqlite3`, the proof-search runtime in `semantic-runtimes/`,
and PID records in `processes/`. It is `LEAN_HOST_MCP_STORE_DIR`, else `[install] store_dir`, else
`.local/lean-host-mcp/store` under the Lake root. The server creates it with mode 0700 once its config checks pass.
`LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`, a canonical absolute path, moves `semantic-runtimes/`, and
`LEAN_HOST_MCP_PROCESS_REGISTRY_DIR` moves `processes/`. One server serves one store. The lock binds the store, not the
checkout, so run one server per checkout and never point two servers with different stores at one Lake root.

The request table uses SQLite's write-ahead log with `synchronous=FULL`, and `fullfsync` on macOS, so a row survives a
crash of the server or the machine; the file has mode 0600. The server prunes the table at start and every hour: ended
rows older than `requests.retention_hours`, then the oldest ended rows past `requests.max_rows`, then their logs, and at
start every log that no row names. It never prunes a row that has not ended. A server without a store refuses request
ids with `requests_unavailable`.

### Configuration

The server reads two TOML files at its Lake root and no others: the tracked `lean-host-mcp.toml`, and the local
`.local/lean-host-mcp/config.toml` that install writes, in a directory Git ignores. The local file wins one key at a
time, so each file sets only what it changes. A missing file adds nothing, and tables the server does not know, such as
an old `[launcher]`, are ignored. A file that does not parse, or a value of the wrong type, stops the server at startup
with an error that names the file or the key.

```sh
lean-host-mcp config init --lake-root PROJECT   # write PROJECT/lean-host-mcp.toml: every default, commented
lean-host-mcp config check --lake-root PROJECT  # name each bad key and value; print [build] and each recipe
```

`config init` takes `--path FILE` for another file and needs `--force` to overwrite one. `config check` runs no Lake and
writes nothing, so it is safe beside a running server.

### Configuration reference

Each setting takes the first value it finds: CLI flag, environment variable, local file, tracked file, built-in default.
An environment variable that is empty or holds only spaces counts as unset.

<!-- BEGIN GENERATED: do not edit by hand. Regenerate from `config_schema::render_reference_table`; the `operations_md_reference_table_is_in_sync` test fails when this block drifts. -->
| Key | Type | Default | Override | Description |
| --- | --- | --- | --- | --- |
| `runtime.lean_max_memory_kib` | integer (KiB) | unset | `LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB` | Lean heap ceiling per worker child. Allocator exhaustion can abort the child when a C++ exception reaches the Rust FFI boundary. Unset derives the residue budget plus max(residue budget, 8 GiB), converted to KiB, leaving headroom above clean import recycling. Startup warns when an explicit ceiling is at or below the residue budget. |
| `runtime.request_timeout_millis` | integer (ms) | `120000` | `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS` | One shared budget for worker-capacity admission, mailbox admission and queued waiting for each worker operation. After dispatch, worker execution has a separate timeout of the same length; execution expiry recycles the worker. Admission expiry does not dispatch that queued operation. This is not a whole-tool deadline: preparation and multiple operations can take longer. Default 120 s. |
| `runtime.project_mailbox_capacity` | integer | `16` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | Maximum queued calls in each worker actor's mailbox. Further callers wait in FIFO order for mailbox space within the same original request_timeout_millis admission deadline. Cancellation removes only that caller. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many abnormal worker restarts (a timeout, a cancellation, a lost child) the supervisor makes within the restart window. It refuses the next one: the worker stays down, and calls fail with retry_after_millis, until the oldest restart leaves the window. Planned cycles do not count. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `runtime.worker_import_residue_budget_mib` | integer (MiB) | unset | `LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB` | Unreclaimable import bytes per worker before a clean recycle. Unset uses a quarter of RAM divided among broker.max_workers, clamped to 9-12 GiB and capped at each worker's RAM share minus 4608 MiB (minimum 1 MiB). This affordability cap can lower the default below 9 GiB. Explicit MiB values bypass RAM sizing; a budget below one import's residue can force frequent recycling. |
| `runtime.worker_session_pool_capacity` | integer | `8` | `LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY` | How many imported Lean environments one worker child keeps warm. Returning to a pooled import profile costs a key comparison instead of a full import, so this should cover the number of distinct per-file import profiles a session moves between — a changed-file sweep touches one per file. A held environment costs about 70 MiB against 2-4 GB for the import it saves. Default 8. |
| `broker.max_workers` | integer | `4` | `LEAN_HOST_MCP_MAX_WORKERS` | Worker processes the server may run at once, counting workers that are starting or still exiting. An exiting worker keeps its slot until its exit is confirmed, so a value above max_workers_per_project lets a new worker start while an old one exits. Default 4. |
| `broker.max_workers_per_project` | integer | `1` | `LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT` | Workers that may serve the project at once. An extra worker opens only when a slot is free; otherwise requests queue on an existing worker. Set 2 only after measuring the memory cost of concurrent checks: each worker keeps its own imports and cache. Default 1. |
| `broker.max_lean_runs` | integer | `2` | `LEAN_HOST_MCP_MAX_LEAN_RUNS` | Short lean and lake runs the server may run at once: file checks, lake setup-file, lake query and lake env runs, and semantic file runs. Each takes a permit before it starts and keeps it until its process group has exited; a run that finds no permit by its deadline does not start. Default 2. |
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
| `install.store_dir` | path | unset | `LEAN_HOST_MCP_STORE_DIR` | Private server state directory (mode 0700). Its lock admits one server at a time; a second server on the same store exits and names the first. It also holds the lock that every process the server starts inherits (children.lock, with one file per running group in children/), the semantic runtime cache that install builds (semantic-runtimes/) and the server process records (processes/). Unset uses .local/lean-host-mcp/store under the Lake root. A relative path is relative to the Lake root. |
| `install.elan_home` | path | unset | `ELAN_HOME` | Elan's home when install ran: ELAN_HOME, else $HOME/.elan. When ELAN_HOME is unset, the server and install-worker --lake-root look for the Lean toolchain here, not under $HOME, and pass it on as ELAN_HOME. |
| `requests.retention_hours` | integer (h) | `168` | none | Hours an ended request keeps its row, its stored result and its build log. The server prunes at start and every hour; it never prunes a request that has not ended. Default 7 days. |
| `requests.max_rows` | integer | `10000` | none | Most rows the request table keeps. Past this, pruning deletes the ended rows that finished first. Default 10000. |
| `requests.wait_max_secs` | integer (s) | `300` | none | Longest wait_seconds one lean_get may ask for. Default 300 s. |
| `build.guard` | array of Lake targets | `[]` | none | Targets that must already be built. A guard step runs lake build --no-build on them, which fails without building anything when one is out of date. It runs before a build of targets; a recipe's guard key places it. A project on Mathlib can use ["@mathlib/+Mathlib:olean"]. Empty runs no guard. |
| `build.timeout_secs` | integer (s) | `14400` | none | Deadline for one build, counted from its request: the wait for the project's write turn and every step count. It also limits install's build of the semantic search runtime and a server's open of it: each waits at most this long for the cache's lock, and then for Lake. Default 4 hours. |
| `build.log_limit_mib` | integer (MiB) | `16` | none | Cap on one build's log, which lives in the store's logs directory. Output past the cap is dropped, and the reply says so. Default 16 MiB. |
| `recipes.<name>.steps` | array of arrays of strings | unset | none | The Lake runs of one recipe, in order. Each is the words after lake and may not be empty; Lake reports its own errors in them. The first run that fails ends the recipe. Steps run in the Lake root with the server's environment. lean_build runs a recipe by its name. |
| `recipes.<name>.append` | string (none, last, each) | `"none"` | none | Which steps get the request's args at their end: none refuses args, last gives them to the last step, each to every step. An arg may not be empty or start with -. Default none. |
| `recipes.<name>.guard` | string (before, after, none) | `"before"` | none | Where the build.guard step runs: before the steps, after them, or not at all. A failing guard before the steps ends the recipe. Default before. |
| `recipes.<name>.timeout_secs` | integer (s) | unset | none | Deadline for one build of this recipe, in place of build.timeout_secs. Unset uses build.timeout_secs. |
<!-- END GENERATED -->

When `output.max_total_bytes` runs out in a batch of proof steps, the current candidate reports `budget_exceeded` and
the later ones `not_attempted`. When a reply's summary counts any `budget_exceeded`, `not_attempted` or
`output_truncated`, retry the best candidate alone or raise the cap.

### Builds

A `lean_build` request names Lake `targets`, none for Lake's default targets, or a `recipe` from the tracked config,
with `args` where the recipe's `append` lets them in:

```toml
[build]
guard = ["@mathlib/+Mathlib:olean"] # must already be built; checked with lake build --no-build
timeout_secs = 3600                 # from the request: the wait for the turn and every step count
log_limit_mib = 16

[recipes.clean]
steps = [["clean", "MyProject"]]
guard = "none"

[recipes.format]
steps = [["build"], ["exe", "lean-fmt", "format", "--check", "--no-build"]]
append = "each"                     # the request's args go at the end of both steps
```

Each step runs as `lake <args>` in the Lake root; the [tool catalog](tool-catalog.md#lean_build) gives the order and the
reply. A step whose `lake` exits while others in its process group still run gets one second; then the server stops the
group, and the step ends the build with no exit code. Steps run with the server's environment, plus `LEAN_SYSROOT` set
to the project's toolchain and `LAKE_ARTIFACT_CACHE=false`, so builds land in each package's own build directory. Set a
variable such as `LEAN_NUM_THREADS` where the server starts. A target or argument from a client may not be empty or
start with `-`, so a client cannot pass Lake an option such as `-d`. Recipe words come from the tracked config, and Lake
reports its own errors in them.

Build only through `lean_build`: a `lake build` run outside the server can rewrite `.olean` files while a check reads
them, and the server does not notice.

### Sizing

Size the worker count first. `broker.max_workers` caps the server's worker processes, counting those starting or still
exiting. `broker.max_workers_per_project` caps those on the project; it defaults to 1, since each worker keeps its own
imports and cache. `broker.max_lean_runs` caps short runs, and each file check among them loads its file's imports.
Proof search runs one more `lean` process, the search driver, beside the worker. `LEAN_RS_NUM_THREADS` on the server
bounds Lean's task threads; the default uses every core.

A worker's memory has two limits:

- `runtime.worker_import_residue_budget_mib` bounds the import bytes a child can never free, and recycles the child
  cleanly once they pass it. The default starts at a quarter of RAM divided by `broker.max_workers`, clamped to 9–12
  GiB, then capped at one worker's share of RAM minus 4.5 GiB, which can put it below 9 GiB.
- `runtime.lean_max_memory_kib` is Lean's heap ceiling. Crossing it can abort the whole child, so it defaults to
  `budget + max(budget, 8 GiB)`. Startup warns when a ceiling you set is at or below the budget.

Plan for `budget + 4.5 GiB + max(budget, 8 GiB)` per worker beside the machine's other work: one worker with a 4 GiB
budget needs about 16.5 GiB. A budget you set does not shrink when the worker count changes. RSS is only reported: it
counts shared `.olean` pages and can fall while the child's private memory grows.

With unchanged builds, repeating one file, or moving among the profiles the session pool holds
(`runtime.worker_session_pool_capacity`), adds no residue. A sweep of new import profiles recycles the child at a rate
set by the size of each import and the budget, not by the number of calls. `import_residue` counts the recycles a call
waited for, and `import_residue_idle` counts those done between calls. When the first count leads, work never pauses
long enough; raise the budget if the machine has room.

### Logs

The server logs to stderr, never to the stdio protocol stream. At the default level each call logs two lines with a
request number, the tool, its kind and the project's base name:

```text
INFO Lean request received (preparing or waiting for worker) request=42 tool="lean_verify" kind="verification" project=myproject
INFO Lean response ready request=42 tool="lean_verify" kind="verification" project=myproject elapsed_ms=1532 outcome="completed" tool_errors=0 tool_warnings=0 verified=3 failed=1 needs_build=0
```

`completed` means a reply is ready, not that a proof checked: read `failed`, `diagnostic_errors` and the reply itself.
The lines hold no source text, names, imports, diagnostics, request ids or full paths. A retry with the same
`request_id`, a `lean_get` and a `lean_runtime` call log nothing. When a call ends before its reply, a warning says that
the worker's end is not confirmed.

Each worker recycle logs one line with `cause`, `reason`, `worker_generation`, `planned` and `restarts_total`. Abnormal
restarts log at `warn`: `timeout`, `cancelled`, `child_abort`, `child_exit`, `session_missing`, `request_failed`,
`start_failed`, `rss_hard_limit` and `worker_internal`. Of the planned ones, `artifacts_rebuilt`, `import_residue`,
`import_accounting_unknown` and `rss_ceiling` log at `info`, and `import_residue_idle`, `max_imports`, `max_requests`,
`idle` and `explicit` at `debug`. The server sets no RSS limit, so the two RSS causes do not fire. For a child that
died, `reason` ends with what the child last wrote to stderr.

The default filter, `info,rmcp=warn`, hides routine MCP session lines and keeps the SDK's warnings. Use
`RUST_LOG=info,rmcp=debug` for transport detail and `RUST_LOG=lean_host_mcp=debug` for the runtime; the estimate of the
import budget appears only at debug. `telemetry.verbosity = "full"` adds runtime facts to each reply under
`telemetry.runtime`: the worker generation, the queue wait, restarts by cause, `import_residue_bytes` against
`import_residue_limit_bytes`, and a retried call's `previous_attempt_failure`.

### Process records

Each running server writes a PID record in the store's `processes/`, or, without `--lake-root`, in the per-user cache
directory's `lean-host-mcp/processes/`, and removes it on a clean exit. `doctor processes` lists the records: the PID,
whether it lives (`true`, `false` or `unknown`), whether the executable matches, the transport, bind and path, the
working directory, the parent PIDs, the process group and the child PIDs. It reads only what servers wrote and never
looks for processes by name.

```sh
lean-host-mcp doctor processes --lake-root PROJECT      # this project's servers
lean-host-mcp doctor processes                          # servers without a Lake root
lean-host-mcp doctor processes --cleanup-stale-records  # drop records of servers that are gone
```

Cleanup removes a record only when the kernel confirms, twice, that its PID is gone; it kills nothing. For a live stdio
server whose parent changed, the listing suggests `kill -TERM <pid>` for that exact PID.

## Upgrade

### Upgrading to 0.19

A 0.19 server and its worker upgrade together, with lean-rs 0.13, lean-semantic-search 0.10 and worker protocol 15.

1. Stop the server and any build on the project. A 0.19 server, and `install`, refuse to start while a 0.18 server holds
   the store.
2. Run `lean-host-mcp install --lake-root PROJECT` with the new binary. It builds a worker in a new generation, since
   another release built the old one, and builds the new proof-search runtime into a new entry in the store's
   `semantic-runtimes/`; the old entry stays there unused. Use `install-worker` for a worker installed on its own.
3. Run `lean-host-mcp config check --lake-root PROJECT`. A config file that does not parse now stops the server at
   startup, where 0.18 served with the defaults.
4. Start the server again. Its first call asks Lake for the import roots, about 0.3 s on a small project, and the server
   asks again after each change to the lakefile, the toolchain pin or the manifest.

Clients:

- `lean_compatibility` is gone, so call `lean_status` with `include: ["worker"]` and read `data.worker.compatibility`.
- A client that lists trust statuses should add `unknown_import`.
- Proof search candidates lose `score`. They come with exact name matches first, then the semantic hits in rank order,
  then the other declaration-search rows.
- Once abnormal worker restarts reach `runtime.worker_restart_limit` within the window, the worker stays down: calls
  fail with a retryable `runtime_unavailable` whose `details.retry_after_millis` says how long to wait, and
  `lean_status` reports `data.worker.restart_retry_after_millis`.
- Restart causes take lean-rs's names: `rss_post_job` is now `rss_ceiling`, `rss_hard_limit_exceeded` is
  `rss_hard_limit`, and `child_exit`, `session_missing`, `request_failed` and `start_failed` appear by name instead of
  as `worker_internal`.
- A restart before a call no longer taints its verdict; only a restart that left the call unfinished does.
- `lean_status` project rows for `olean` and `ilean` read `unknown` until Lake has named the import roots since the
  server started or the Lake configuration changed.

### Installing workers

`install` covers the usual case. `install-worker` builds workers alone: into `--workers-dir DIR`, or into the worker
directory of `--lake-root PROJECT`, which is `LEAN_HOST_MCP_WORKERS_DIR`, then `[install] workers_dir`, then
`PROJECT/.local/lean-host-mcp/workers`. With neither flag it uses `LEAN_HOST_MCP_WORKERS_DIR` and fails when that is
unset. With `--lake-root` it takes the project's store, except for `--list`, so it refuses while a server serves the
project. With only `--workers-dir` it takes no store: stop every server that uses the directory first.

A binary from `cargo install` builds the published `lean-host-mcp-worker` at its own version. A binary built in a
checkout builds the worker from that checkout, and `--source-dir DIR` names another. Each worker's rpath points at its
toolchain's `lib/lean`.

Each worker lives in `DIR/<id>/generation-<name>/`, and `DIR/<id>/current-generation` names the one the server runs. A
new generation writes and builds its shims with plain Lake, records their digests in `worker.json`, passes the smoke
test, and only then becomes current; a failure leaves the old one current.

- `--auto`, the default, builds a worker for each toolchain in elan's `toolchains/` whose worker is missing or stale,
  and skips current ones and those outside the supported window. `--force` rebuilds current ones too.
- `--toolchain <id>` builds one toolchain's worker.
- `--list` prints each toolchain's current generation. Its `protocol` column reads `compatible` or `incompatible`, and
  the other columns read `unknown` when the record does not load.
- `--clean` removes every worker, or one with `--toolchain <id>`. It never touches source.
- `--prune` removes only workers outside the supported window. `--list` and `--prune` skip a directory with no
  `current-generation`.

### Elan toolchains

Keep a toolchain installed and unchanged while its workers run, and stop the servers that use it before you uninstall or
replace it. The server checks the toolchain when it admits a call and takes no lock on it.

## Recover

### A refused start

When another server holds the store, the new one exits with:

```text
another lean-host-mcp (pid 4242, lake root /work/project) holds the store /work/project/.local/lean-host-mcp/store; stop it first
```

Stop that server by its pid, or use it.

When processes of an earlier server still hold `children.lock` after the server's wait, it exits with:

```text
processes an earlier server started still hold /work/project/.local/lean-host-mcp/store/children.lock after 35 s: process groups 51234 still run. Processes the server does not record, such as workers whose pipe to the dead server has not closed yet, may hold it too. `lsof /work/project/.local/lean-host-mcp/store/children.lock` lists every holder; stop them, then start the server again
```

1. List every holder with `lsof STORE/children.lock`.
2. Check each with `ps -o pid,pgid,command -p PID`. Expect `lake`, `lean` or `lean-host-mcp-worker` processes that the
   dead server started.
3. Stop each by its exact pid, or its process group when the error names it: `kill -TERM -- -PGID`, then
   `kill -KILL -- -PGID` if it does not exit. Never stop processes by name.
4. Start the server again. It deletes what the dead server left in `children/`.

### A refused worker

A missing, stale or incompatible worker refuses before it starts, with one line that names the worker directory and ends
with the command that fixes it:

```text
no worker for leanprover/lean4:v4.36.0 in /work/project/.local/lean-host-mcp/workers (an install by an older lean-host-mcp must be redone); run: /work/project/.local/lean-host-mcp/bin/lean-host-mcp install --lake-root /work/project
```

Stop the server, run that command, and start the server again. A protocol mismatch is a `runtime_unavailable` reply that
is not retryable and carries the same command. A proof search that finds its runtime unbuilt says to run `install`
again.

### A check that needs a build

Run the `lean_build` call that a `needs_build` row names, then retry; an `unknown_import` row means fix the import
([response shape](tool-catalog.md#response-shape)). A failure at stage `lake_option_discovery` means Lake could not
answer for the project's configuration, and no proof check ran. Right after a lakefile edit, a check can fail with
Lake's "could not acquire an exclusive configuration lock". Retry it.

### A lost reply

Send the call again with the same `request_id`, or call `lean_get` with it. The server runs each id once and keeps its
result; the [tool catalog](tool-catalog.md#named-requests) gives the states.

### Runtime errors

A runtime or controller failure a client can act on returns `data: null` and an issue in `errors` with
`code: "runtime_unavailable"`, a reason, a `retryable` flag and `details`. Follow the flag and any recovery command.

| Failure | Where it appears |
| --- | --- |
| Lean parse or elaboration errors, kernel rejection, meta timeout | `data`; a failed proof is a completed call |
| Admission timeout, worker death, session loss | `errors` with `code: "runtime_unavailable"`; retryable or not by outcome |
| Heap-ceiling overrun | Can abort the child and enter worker-death recovery, with no Lean diagnostic |
| I/O and config errors, broken invariants, an unusable Lake project | MCP error |

An admission timeout, `project_admission_timeout`, `mailbox_admission_timeout` or `queue_wait_timeout`, means the call
never ran: retry it, or raise `runtime.request_timeout_millis` or `broker.max_lean_runs` when calls often wait. A fresh
retry that completes returns usable data with a warning about the failed first attempt; a failed or uncertain retry
returns none.

A reason that starts with `restart_limit_exceeded` means `runtime.worker_restart_limit` abnormal restarts fell within
`runtime.worker_restart_window_secs`, so the worker stays down. Wait `details.retry_after_millis`, then retry; the
`warn` lines in the log name each restart's cause ([logs](#logs)).
