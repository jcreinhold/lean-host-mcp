# Operations

Operational reference for `lean-host-mcp`: tuning knobs, transport internals, the full runtime-error contract, and the
test and performance harness. Most users need none of this — start with the [README](../README.md) and the
[tool catalog](tool-catalog.md). Reach for this page when you are sizing a deployment, debugging a retryable runtime
issue in `errors`, or working on the server itself.

## Configuration file

Every knob can live in one TOML file instead of a dozen environment variables. Generate a documented starter — every
option written at its current default, each with a comment explaining it — then edit what you need:

```sh
lean-host-mcp config init          # writes ./lean-host-mcp.toml (project-local)
lean-host-mcp config init --home   # writes ~/.config/lean-host-mcp/config.toml (per-user)
```

`config init` refuses to overwrite an existing file unless you pass `--force`; `--path FILE` writes somewhere else.
Discovery at startup:

1. **Project-local** (preferred): the nearest `lean-host-mcp.toml`, found by walking up from the server's working
   directory (the same upward search as the lakefile).
2. **Home**: `<config-dir>/lean-host-mcp/config.toml` (e.g. `~/.config/lean-host-mcp/config.toml`;
   `LEAN_HOST_MCP_CONFIG_DIR` overrides the base dir, used by the test suite).

When both exist they **merge per key**: the home file sets baseline values and the local file overrides only the keys it
sets. A missing file is fine; a malformed file is logged and ignored. The same startup validation applies whatever the
source (RSS ordering, non-zero pool guards).

## Configuration reference

Every knob, with the environment variable — and, for the transport knobs, the CLI flag — that overrides it. Precedence
per knob is **CLI flag > env var > file > built-in default**, so an env var still overrides the file and existing
`LEAN_HOST_MCP_*` setups keep working unchanged. RSS thresholds are in KiB and byte caps in bytes; a magnitude in a
description (e.g. "5 GiB") is for reading, not for setting.

<!-- BEGIN GENERATED: do not edit by hand. Regenerate from `config_schema::render_reference_table`; the `operations_md_reference_table_is_in_sync` test fails when this block drifts. -->

| Key | Type | Default | Override | Description |
| --- | --- | --- | --- | --- |
| `primary_project` | path | unset | `--lake-root / LEAN_HOST_MCP_PROJECT` | Default Lake project for calls that omit an explicit project= argument. Lowest-priority fallback, after the flag/env and the nearest lakefile above the working directory. |
| `runtime.worker_rss_post_job_restart_kib` | integer (KiB) | `5242880` | `LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB` | Post-job soft restart ceiling: after a call finishes, the worker is recycled if its resident memory is at or above this. Raise toward the hard-kill ceiling to recycle less often. Default 5 GiB. |
| `runtime.worker_rss_hard_kill_kib` | integer (KiB) | `16777216` | `LEAN_HOST_MCP_WORKER_RSS_HARD_KILL_KIB` | In-flight hard-kill ceiling: a call whose worker crosses this is killed mid-call so a runaway tactic cannot exhaust the machine. Must be at least the post-job ceiling. Default 16 GiB. |
| `runtime.worker_rss_sample_millis` | integer (ms) | `250` | `LEAN_HOST_MCP_WORKER_RSS_SAMPLE_MILLIS` | How often the supervisor samples worker resident memory for the in-flight hard-kill watchdog. |
| `runtime.import_switch_rss_soft_kib` | integer (KiB) | `2097152` | `LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB` | Soft restart ceiling applied when a call needs a different import set than the live worker holds. Must not exceed the post-job ceiling. Default 2 GiB. |
| `runtime.module_cache_rss_guard_kib` | integer (KiB) | `2097152` | `LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB` | Resident-memory ceiling above which the per-worker module-query cache stops growing. Default 2 GiB. |
| `runtime.module_cache_max_bytes` | integer (bytes) | `33554432` | `LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES` | Maximum size of the per-worker module-query result cache, in bytes. Default 32 MiB. |
| `runtime.request_timeout_millis` | integer (ms) | `120000` | `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS` | Per-request worker deadline covering one tool call end to end. On expiry the worker is recycled and the call returns a retryable runtime error. Raise it for unusually heavy modules whose lean_verify/lean_context work legitimately runs longer; lower it to bound a single heavy file query. Default 120 s. |
| `runtime.project_mailbox_capacity` | integer | `8` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | How many calls may queue for one project's worker before new calls are shed with a retryable busy status. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many worker restarts are tolerated within the restart window before the project is marked unhealthy. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `broker.max_projects` | integer | `4` | `LEAN_HOST_MCP_MAX_PROJECTS` | How many distinct Lake projects stay open at once; on overflow the least-recently-used project's worker is evicted. |
| `broker.idle_timeout_secs` | integer (s) | `600` | `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Evict a project's worker after this many idle seconds. 0 disables idle eviction. Default 10 minutes. |
| `broker.semantic_permits` | integer | `1` | `LEAN_HOST_MCP_SEMANTIC_PERMITS` | How many semantic (elaborating) calls run concurrently across all projects and parallel server processes sharing the semantic lock directory. |
| `broker.semantic_waiters` | integer | `16` | `LEAN_HOST_MCP_SEMANTIC_WAITERS` | How many semantic calls may queue for a permit before new ones are shed with a retryable semantic_admission_full status. |
| `broker.semantic_admission_timeout_millis` | integer (ms) | `60000` | `LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS` | How long a semantic call waits for a permit before giving up with a retryable semantic_admission_timeout status. Default 60 seconds. |
| `broker.semantic_lock_dir` | path | unset | `LEAN_HOST_MCP_SEMANTIC_LOCK_DIR` | Directory for OS-visible cross-process semantic admission locks. Unset uses the per-user cache directory. Parallel servers sharing a directory must agree on broker.semantic_permits. |
| `server.bind` | string (loopback ADDR:PORT) | unset | `--bind / LEAN_HOST_MCP_BIND` | Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS. |
| `server.http_path` | string | unset | `--http-path / LEAN_HOST_MCP_HTTP_PATH` | HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp. |
| `server.response_carrier` | string (text, structured, both) | `"text"` | `LEAN_HOST_MCP_RESPONSE_CARRIER` | Which field of the tool result carries the semantic response. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text. |
| `telemetry.verbosity` | string (quiet, full) | `"quiet"` | `LEAN_HOST_MCP_TELEMETRY_VERBOSITY` | How much operational telemetry the internal operation envelope keeps before semantic response adaptation. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet. |
| `output.max_field_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES` | Override the per-field output byte cap for all tools. Unset keeps each tool's built-in default (8 KiB for inspection, 4 KiB for proof actions). Clamped to 256 bytes to 64 KiB. |
| `output.max_total_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES` | Override the total output byte cap for all tools. Unset keeps the built-in 64 KiB default. Clamped to 1 KiB to 64 KiB. |
| `output.heartbeat_limit` | integer (heartbeats) | unset | `LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT` | Default elaboration heartbeat budget for lean_trial proof_step and lean_verify explicit. Unset uses the worker default. Bounds runaway tactics. |

<!-- END GENERATED -->

The three RSS ceilings must satisfy `import_switch <= post_job <= hard_kill`; the server **refuses to start** with a
clear `invalid RSS config: …` message otherwise (an inverted order makes the cheaper planned cycle unreachable — e.g.
`post_job` above `hard_kill` means every overrun escalates straight to an in-flight hard kill). To recycle less often
under memory pressure, raise `post_job` toward (but below) `hard_kill` — e.g.
`LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB=8388608` for an 8 GiB post-job ceiling.

`broker.semantic_permits` is enforced across all `lean-host-mcp` processes sharing `broker.semantic_lock_dir`, not just
inside one server process. The default lock namespace lives under the current user's cache directory at
`lean-host-mcp/semantic-admission`; set `LEAN_HOST_MCP_SEMANTIC_LOCK_DIR` only when you need an explicit namespace.
Permit files are visible as `permit-000.lock`, `permit-001.lock`, and so on, with best-effort holder metadata. Parallel
servers are serialized only when they share the same lock directory. Servers that share a lock directory must also agree
on `broker.semantic_permits`; a process that requests a different count while another permit is active is rejected with
`semantic_admission_config`. Stop existing servers or choose a fresh lock directory before changing the limit.

The admission boundary covers every path that may open, spawn, restart, or run a Lean worker. Cheap metadata paths use
the Lake files directly and do not acquire a semantic permit: degraded `needs_build` responses, invalid-request
responses, and project-scope `.ilean` reference reads can report project identity without opening a worker. File-scope
reference lookup and all declaration/proof operations still acquire a permit before the project can open.

## Observing worker recycles

Every recycle is logged to **stderr** (stdout stays clean for the stdio transport) and tallied into each tool response's
`runtime` facts, so you can answer *why* and *how often* a worker recycles without guessing.

Log lines carry structured fields — `cause`, `reason`, `worker_generation`, `rss_kib`, `limit_kib`, `planned`,
`restarts_total`. Level tracks the signal, not whether the cycle was planned:

- `warn` — abnormal/crash causes: `rss_hard_limit_exceeded`, `child_abort`, `child_exit`, `session_missing`,
  `worker_internal`, `timeout`, `cancelled` (and `restart limit exceeded; marking project unhealthy`).
- `info` — memory-pressure cycles: `rss_post_job`, `rss_import_switch` (the frequency to watch when tuning the budget),
  plus `opened project` / `idle reaper evicted projects` lifecycle lines.
- `debug` — pure hygiene (`max_requests`, `max_imports`, `idle`, `explicit`), per-call tool entry, project resolution,
  the `job` span, and a `post-job rss check` showing live RSS vs the `post_job` ceiling.

Default level is `info`; set `RUST_LOG=lean_host_mcp=debug` for the per-call detail. Example at default level:

```text
INFO worker recycled (memory pressure) cause=rss_post_job rss_kib=Some(7340032) limit_kib=Some(5242880) restarts_total=4
```

The same data reaches the MCP client in `response.runtime`: the per-call cause in `call_restart`, the most recent in
`last_restart`, and the lifetime frequency in `restarts_total` plus the per-cause breakdown `restarts_by_cause` (omitted
when no recycle has happened).

## Process lifetime

The idle reaper (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`) governs resident per-project controllers, not the parent server
process. A stdio server exits when its transport closes: it serves until the client closes the server's stdin. An HTTP
server exits on Ctrl-C, SIGTERM, or ordinary process shutdown. Both transport exit paths call
`ProjectBroker::shutdown_all`, which closes resident projects before the process returns.

Project shutdown is bounded by the worker layer. The host stops accepting new project work, queued messages receive
`runtime_unavailable` with reason `project_shutting_down`, and the controller then lets `lean-rs-worker-parent` perform
its structured child shutdown: terminate, bounded graceful wait, kill escalation if needed, and reap. An active request
may finish normally or run until the configured request timeout before the worker layer reports a terminal runtime
outcome. Abrupt parent death can still skip Rust `Drop`; child-side parent-loss handling is best effort, and stronger
containment remains a launcher or process-manager responsibility.

Every running server writes a PID record under the per-user cache directory at `lean-host-mcp/processes/` and removes it
on normal shutdown. Inspect records with:

```sh
lean-host-mcp doctor processes
```

The output lists only host-written records: PID, liveness, executable-match status when the platform exposes it,
transport, bind/path, working directory, and direct child PIDs. It does not scan for process names. Clean records left
behind by abruptly killed servers with:

```sh
lean-host-mcp doctor processes --cleanup-stale-records
```

Cleanup removes records whose PID is no longer alive. It does not kill live processes and does not infer ownership from
an executable name, command substring, or port number.

## Runtime-error contract

Every public tool returns the same semantic shape (see the [README](../README.md#response-shape)). Recoverable runtime
and project-controller failures are normal tool responses with `data: null` and a structured issue in `errors`, not
JSON-RPC errors:

```jsonc
{
  "data": null,
  "errors": [
    {
      "code": "runtime_unavailable",
      "message": "semantic_admission_timeout",
      "severity": "error",
      "retryable": true,
      "details": {
        "reason": "semantic_admission_timeout",
        "project_root": "/abs/path",
        "session_id": "uuid",
        "worker_generation": 3,
        "worker_restarted": false,
        "restart_cause": null,
        "rss_kib": 2097152,
        "limit_kib": null,
        "retry_after_millis": 60000,
        "restarts_in_window": 1,
        "window_millis": 60000
      }
    }
  ],
  "trust": {
    "project_root": "/abs/path",
    "session_id": "uuid",
    "lean_toolchain": "leanprover/lean4:v4.31.0-rc2",
    "artifacts": [
      {
        "artifact": "worker",
        "scope": "toolchain",
        "status": "unknown",
        "detail": "worker runtime was unavailable for this request"
      }
    ]
  }
}
```

Warnings and next actions from operation-level results are warning issues:

```jsonc
{
  "code": "warning",
  "message": "the project may not be fully built...",
  "severity": "warning",
  "next_action": "lake build # complete the project environment, then retry"
}
```

Which failures land where:

- **Lean-domain failures** — parse errors, elaboration diagnostics, kernel rejection, meta timeout — are part of `data`.
  A failed proof is a successful tool call.
- **Retryable runtime failures** — admission pressure, mailbox pressure (`busy`), worker death, session loss, RSS
  hard-kill — are `errors` with `code: "runtime_unavailable"` and `retryable: true`.
- **MCP errors** are reserved for I/O and config failures, internal-invariant violations, and unusable Lake projects.

### Internal runtime facts

The operation layer still computes freshness/import and runtime facts before semantic response adaptation. Runtime facts
include worker generation, whether a call observed or performed a restart, retry count, admission wait, controller queue
wait, RSS when available, import profile, profile-switch count, and restart history. These remain telemetry and are
omitted at the default quiet verbosity. Proof-relevant artifact facts are public under `trust.artifacts` and survive the
quiet telemetry gate: source snapshots (`source` / `file` / `edit_fresh`), build artifacts (`olean` or `ilean` with
`build_fresh`, `stale_build`, or `missing_build`), and worker/toolchain availability facts.

## Capability shims and module queries

`lean_context(kind = "proof_position")` is the common proof-agent context call: one request returns compact diagnostics,
goals, locals, expected type, target declaration, and the surrounding declaration. It depends on the optional bounded
`lean_rs_host_process_module_query_batch` shim; a worker whose bundled shims lack that capability answers
`{ "status": "unsupported" }`. No public tool requests or caches whole-file info trees. Successful responses carry
`query_facts` (worker cache status, output bytes, phase timings), and repeated calls reach the worker snapshot cache, so
warm behavior is observable.

Files whose header imports modules the server's open env doesn't have are still processed; missing imports surface as an
semantic warning issue. Files using Lean 4's module-system header syntax — `module`, `public import`, `import all`, and
`meta import` — are supported. A header that doesn't parse short-circuits to `header_parse_failed`.

Unlike an external LSP process, the host can still start when unrelated project modules are broken. Calls whose imports
avoid the broken module continue to work; a broken target file reports structured Lean diagnostics instead of a
bootstrap failure.

## Installing workers

`install-worker` always compiles the worker locally, once per Lean toolchain — its `build.rs` bakes an absolute rpath to
that toolchain's `lib/lean`, so a worker binary can't be shipped prebuilt. What it can vary is where the worker *source*
comes from, and it decides that itself:

- **Registry** (the default for a `cargo install lean-host-mcp` binary): fetch and build the published
  `lean-host-mcp-worker` crate at the server's own version (`cargo install lean-host-mcp-worker --version =<ver>`).
- **Local workspace** (automatic when running from a checkout): build the worker from the workspace source
  (`cargo build -p lean-host-mcp-worker`), reusing cargo's incremental cache.

The detection is "was this binary built from a checkout that still has the worker crate beside it?" — no flag needed.
`--source-dir <path>` overrides it to build from an explicit checkout (useful if the original checkout moved after the
binary was built). Either way the worker needs a Rust toolchain on `PATH` and the matching Lean toolchain installed via
elan; the freshly built worker is smoke-tested before it is recorded as usable.

### Keeping workers in step with the host

The worker and the parent share the workspace version and are protocol/ABI-coupled in lockstep: a worker built by a
different `lean-host-mcp` may speak a different worker protocol. **After upgrading `lean-host-mcp`, rebuild your
workers** — otherwise a skewed worker can fail at call time rather than with a clear message.

The provenance sidecar records the building host version, so the tools can tell a worker is stale without running it:

- `install-worker --auto` (the default) scans `~/.elan/toolchains` and (re)builds any worker that is **missing or
  stale** — host-version skew, `lean.h` header drift, or a failed/absent runtime smoke record — and skips ones that are
  current. Out-of-window toolchains are skipped (a worker for them could never load). `--force` rebuilds current workers
  too (e.g. to re-run the smoke test or replace a corrupted binary).
- `install-worker --toolchain <id>` builds one worker, always overwriting.
- `install-worker --list` prints every installed worker; the `host` column reads `current` (built by the running host),
  `stale` (a different, version-locked host — rebuild), or `unknown` (sidecar predates the field).
- `install-worker --clean` removes all installed workers; `--clean --toolchain <id>` removes just one. Workers are
  rebuildable artifacts, so this only deletes from the install root and never touches source. Use it for disk hygiene or
  to force a clean rebuild after a `lean-rs` ABI change.
- `install-worker --prune` removes only *unservable* workers — those outside the supported window or with a recorded
  smoke-test failure. Servable-but-stale workers (header drift, host skew) are kept; rebuild those with `--auto`.

At runtime, a project served by a host-skewed worker still opens but every response carries a warning naming the worker
and host versions and the rebuild command; header drift and smoke failure remain hard refusals.

## Build, test, lint

```sh
cargo build -p lean-host-mcp                          # parent only
cargo build -p lean-host-mcp-worker                   # worker only (links libleanshared)
cargo clippy --workspace --all-targets -- -D warnings # safe; clippy doesn't link
cargo test -p lean-host-mcp                           # unit tests; no Lean fixture required
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
    cargo test -p lean-host-mcp --test e2e -- --ignored   # opt-in end-to-end
```

Build per-member (`-p <name>`); avoid `cargo build --workspace`, which unifies the `lean-rs-sys` feature set across
members and silently links `libleanshared` into the parent. The invariant is asserted by:

```sh
! otool -L target/release/lean-host-mcp | grep -q libleanshared    # macOS
! ldd  target/release/lean-host-mcp | grep -q libleanshared        # Linux
```

## Smoke and performance baseline

The ignored `smoke_perf` integration test is the black-box baseline harness for proof-agent work. It starts the compiled
stdio MCP server, calls `tools/list`, runs representative tool calls, and emits JSONL rows with wall time, serialized
response bytes, 32 KiB / 64 KiB budget flags, status, warning count, observable project-session changes, and process RSS
when the platform exposes it. For `lean_context(kind = "proof_position")`, rows also include the worker module-cache
status, worker-reported output bytes, phase timings, and optional worker cache size facts. The budget constants are
test-only guardrails:
ordinary model-facing responses should aim for 16–32 KiB, with 64 KiB as the default hard ceiling. Production truncation
is still tool-specific policy.

```sh
cargo build -p lean-host-mcp
cargo test -p lean-host-mcp --test smoke_perf -- --ignored --nocapture

LEAN_HOST_MCP_SMOKE_PROJECT=/path/to/your/lake/project \
  LEAN_HOST_MCP_SMOKE_FILE=Relative/Module/File.lean \
  LEAN_HOST_MCP_SMOKE_DECLARATION=Your.Namespace.declaration \
  cargo test -p lean-host-mcp --test smoke_perf -- --ignored --nocapture
```

The harness deliberately does not claim speedups. Keep its JSONL output with any performance change so later comparisons
use the same workload, byte accounting, and cold/warm worker behaviour.
