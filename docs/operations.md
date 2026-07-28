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
source (non-zero guards on every budget and pool size).

## Configuration reference

Every knob, with the environment variable — and, for the transport knobs, the CLI flag — that overrides it. Precedence
per knob is **CLI flag > env var > file > built-in default**, so an env var still overrides the file and existing
`LEAN_HOST_MCP_*` setups keep working unchanged. Memory budgets are in KiB and byte caps in bytes; a magnitude in a
description (e.g. "8 GiB") is for reading, not for setting.

<!-- BEGIN GENERATED: do not edit by hand. Regenerate from `config_schema::render_reference_table`; the `operations_md_reference_table_is_in_sync` test fails when this block drifts. -->

| Key | Type | Default | Override | Description |
| --- | --- | --- | --- | --- |
| `primary_project` | path | unset | `--lake-root / LEAN_HOST_MCP_PROJECT` | Default Lake project for calls that omit an explicit project= argument. Lowest-priority fallback, after the flag/env and the nearest lakefile above the working directory. |
| `runtime.lean_max_memory_kib` | integer (KiB) | `8388608` | `LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB` | Lean heap ceiling for each worker child, enforced inside Lean rather than by watching the process. An elaboration that crosses it fails as an ordinary Lean error inside the tool result, so the worker is not killed and other calls are unaffected. This replaced four resident-memory thresholds, which measured shared mmapped .olean pages and so fired on healthy workers. Default 8 GiB. |
| `runtime.request_timeout_millis` | integer (ms) | `120000` | `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS` | Per-request worker deadline covering one tool call end to end. On expiry the worker is recycled and the call returns a retryable runtime error. Raise it for unusually heavy modules whose lean_verify/lean_context work legitimately runs longer; lower it to bound a single heavy file query. Default 120 s. |
| `runtime.project_mailbox_capacity` | integer | `16` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | How many calls may queue for one project's worker before new calls are shed with a retryable busy status. This is the server's only admission mechanism; it applies per project, so distinct projects never contend for one budget. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many worker restarts are tolerated within the restart window before the project is marked unhealthy. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `broker.max_projects` | integer | `4` | `LEAN_HOST_MCP_MAX_PROJECTS` | How many distinct Lake projects stay open at once; on overflow the least-recently-used project's worker is evicted. |
| `broker.idle_timeout_secs` | integer (s) | `600` | `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Evict a project's worker after this many idle seconds. 0 disables idle eviction. Default 10 minutes. |
| `server.bind` | string (loopback ADDR:PORT) | unset | `--bind / LEAN_HOST_MCP_BIND` | Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS. |
| `server.http_path` | string | unset | `--http-path / LEAN_HOST_MCP_HTTP_PATH` | HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp. |
| `server.response_carrier` | string (text, structured, both) | `"text"` | `LEAN_HOST_MCP_RESPONSE_CARRIER` | Which field of the tool result carries the semantic response. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text. |
| `telemetry.verbosity` | string (quiet, full) | `"quiet"` | `LEAN_HOST_MCP_TELEMETRY_VERBOSITY` | How much operational telemetry the internal operation envelope keeps before semantic response adaptation. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet. |
| `output.max_field_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES` | Override the per-field output byte cap for all tools. Unset keeps each tool's built-in default (8 KiB for inspection, 4 KiB for proof actions). Clamped to 256 bytes to 64 KiB. |
| `output.max_total_bytes` | integer (bytes) | unset | `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES` | Override the total output byte cap for all tools. Unset keeps the built-in 64 KiB default. Clamped to 1 KiB to 64 KiB. |
| `output.heartbeat_limit` | integer (heartbeats) | unset | `LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT` | Default elaboration heartbeat budget for lean_trial proof_step and lean_verify target groups. Unset uses the worker default. Bounds runaway tactics. |

<!-- END GENERATED -->

`output.max_field_bytes` and `output.max_total_bytes` bound model-facing payloads before they leave the worker. Tight
caps are useful for smoke tests and for very large proof states, but they can also make proof-step batches partial: once
the total proof-action budget is exhausted the current candidate reports `budget_exceeded` and later candidates report
`not_attempted`. Retry the promising snippet alone, or raise `LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES`, when the response
summary shows nonzero `budget_exceeded`, `not_attempted`, or `output_truncated` counts.

### Rebuilt artifacts

A worker session's Lean environment is a **snapshot taken at import**. The child reuses a session whose imports match
the request, so `.olean` files written after that import are invisible to it — a `lake build` in another terminal would
otherwise keep being answered from the pre-build environment. (Before the worker child learned to reuse sessions it
re-imported on every call, and that accident is what used to hide this.)

The server therefore stamps the newest modification time among the *imported modules'* own `.olean` files when it opens
a session, re-stamps before each call that would reuse it, and recycles the worker when the stamp has advanced. The
recycle happens before the job runs, so the call is answered by a fresh worker and the result is sound; it appears as an
`artifacts_rebuilt` entry in `runtime.call_restart` and one `info` log line. The cost is one `stat` per import per call.

Two limits worth stating. Only the modules a call names in its import set are watched, not their full transitive closure
— nearly the same set in practice, because Lake's traces include each dependency's hash, so rebuilding a dependency
rebuilds the `.olean` of everything importing it. And the check compares mtimes, so a build that leaves every imported
artifact byte- and time-identical is correctly treated as no change.

### Memory

`runtime.lean_max_memory_kib` is the only memory *knob*, and it bounds the wrong-quantity problem out of existence.
Earlier releases carried four *resident*-memory thresholds — an import-switch soft cycle, a post-job recycle, an
in-flight hard kill, and a forced recycle every 64 requests. Resident memory counts the shared, clean, mmapped `.olean`
pages a Mathlib-scale worker maps at startup, so those thresholds fired on healthy workers while doing nothing about the
process that was genuinely growing. One cause of that growth — the worker child re-importing on every call — is fixed at
the source: a call whose imports match the live session reuses it, so nothing accumulates as long as a client keeps
asking about the same imports.

What remains is a ceiling on the Lean **heap**, enforced inside Lean by `lean_internal_set_max_memory` rather than by
watching the process from outside. Crossing it is an ordinary elaboration failure inside the tool result: the worker
keeps running, concurrent calls are unaffected, and the response says which declaration was too expensive. Raise it if a
legitimately heavy module reports memory exhaustion; lower it to make a runaway tactic fail sooner.

### Import cycling

Importing is the one thing that still accumulates. A Lean environment imported with `loadExts := true` cannot be
reclaimed, so every import a child performs is retained until that child exits — measured on the bundled fixture at
roughly **+1 GiB per import**, reaching 11.2 GiB and an OS `SIGKILL` before call 180 when nothing bounded it.

The worker child therefore keeps a small pool of imported sessions rather than dropping the outgoing one when the
imports change. Returning to a profile it still holds is a key comparison, not an import: an alternating workload
imports once per **distinct** profile instead of once per switch. Holding an environment alive rather than dropping it
costs 30–50 MB, against the 0.8–1.0 GiB the import that produced it costs, because the regions outlive the environment
either way.

The server cycles a worker after a small number of imports, which — the pool being the same size — is also the number of
distinct import profiles one child may hold. This is not tunable and needs no attention in normal use. A client working
one proof loop repeats one import profile and never triggers it, because a reused session is not an import; a client
that alternates among a handful of profiles does not trigger it either, because a pooled session is not an import. A
client that rotates through more profiles than the pool holds sees a `max_imports` entry in `runtime.call_restart`,
logged at `debug`. Those cycles are planned — they do not count toward `runtime.worker_restart_limit`, and the call that
triggers one still answers normally. If you see `max_imports` dominating your logs, the fix is on the client side: draw
related calls from a smaller set of `imports` lists rather than varying it per call.

## Concurrency and admission

There is no process-wide or cross-process admission gate. Each resident project owns one worker child and one dedicated
thread that runs a single job at a time, so a project's work is serialized by construction;
`runtime.project_mailbox_capacity` bounds how many calls may wait for that thread before further calls are shed with a
retryable `mailbox_full` status. Server-wide concurrency is therefore `broker.max_projects` workers, one in-flight call
each — calls against *different* projects run at the same time and never queue behind one another. Parallel
`lean-host-mcp` processes are likewise independent; if a machine cannot hold that many workers, lower
`broker.max_projects`.

Cheap metadata paths never open a worker at all: degraded `needs_build` responses, invalid-request responses, and
project-scope `.ilean` reference reads answer from the Lake files directly. A repeat module query on an unmodified file
is answered from the per-project result cache and likewise never enters the mailbox, so a warm call does not queue
behind in-flight Lean work.

**Semantic search costs a second child, briefly.** `search_for_proof`'s semantic ranking runs in its own worker child,
separate from the one serving elaboration. It is spawned for the call and released when the call ends, so two children
are alive only for the duration of a semantic search and the steady-state footprint stays at one worker per resident
project. Keeping that child resident between calls was tried and measured *slower*; the note on
`open_semantic_capability` in `src/project.rs` records why.

## Observing worker recycles

Every recycle is logged to **stderr** (stdout stays clean for the stdio transport) and tallied into each tool response's
`runtime` facts, so you can answer *why* and *how often* a worker recycles without guessing.

Log lines carry structured fields — `cause`, `reason`, `worker_generation`, `rss_kib`, `limit_kib`, `planned`,
`restarts_total`. Level tracks the signal, not whether the cycle was planned:

- `warn` — abnormal/crash causes: `rss_hard_limit_exceeded`, `child_abort`, `child_exit`, `session_missing`,
  `worker_internal`, `timeout`, `cancelled` (and `restart limit exceeded; marking project unhealthy`).
- `info` — `artifacts_rebuilt` (a `.olean` among the imports of a session the child may still hold was rewritten, so the
  child was recycled), memory-pressure cycles (`rss_post_job`), plus `opened project` / `idle reaper evicted projects`
  lifecycle lines.
- `debug` — pure hygiene (`max_imports`, `max_requests`, `idle`, `explicit`), per-call tool entry, project resolution,
  and the `job` span.

The server no longer configures any resident-memory threshold, so in practice you see the abnormal causes above,
`explicit`, `artifacts_rebuilt`, and `max_imports` (see "Import cycling"). `rss_post_job` / `rss_hard_limit_exceeded`
and `max_requests` stay in the vocabulary because the supervisor still reports them if a future policy asks for them;
seeing one today means the worker was configured somewhere other than here.

Default level is `info`; set `RUST_LOG=lean_host_mcp=debug` for the per-call detail. Example at default level:

```text
INFO worker recycled (imports rebuilt on disk) cause=artifacts_rebuilt worker_generation=2 planned=true restarts_total=1
```

The same data reaches the MCP client in `response.runtime`: the per-call cause in `call_restart`, the most recent in
`last_restart`, and the lifetime frequency in `restarts_total` plus the per-cause breakdown `restarts_by_cause` (omitted
when no recycle has happened).

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

The output lists only host-written records: PID, liveness, executable-match status when the platform exposes it,
transport, bind/path, working directory, startup/current parent PID, process group, stale-stdio-client status, and
direct child PIDs. It does not scan for process names. If a live stdio server is reparented, the output suggests an
exact PID command such as `kill -TERM <pid>` for the recorded server PID. Clean records left behind by abruptly killed
servers with:

```sh
lean-host-mcp doctor processes --cleanup-stale-records
```

Cleanup removes records whose PID is no longer alive. It does not kill live processes and does not infer ownership from
an executable name, command substring, or port number. Do not use broad process-name cleanup; identify the exact PID
from the registry or from `ps -axo pid,ppid,pgid,stat,rss,command` before terminating a live process.

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
      "message": "mailbox_full",
      "severity": "error",
      "retryable": true,
      "details": {
        "reason": "mailbox_full",
        "project_root": "/abs/path",
        "session_id": "uuid",
        "worker_generation": 3,
        "worker_restarted": false,
        "restart_cause": null,
        "rss_kib": 2097152,
        "limit_kib": null,
        "retry_after_millis": null,
        "restarts_in_window": 1,
        "window_millis": 60000
      }
    }
  ],
  "trust": {
    "project_root": "/abs/path",
    "session_id": "uuid",
    "lean_toolchain": "leanprover/lean4:v4.33.0-rc1",
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
- **Retryable runtime failures** — mailbox pressure (`busy`), project-pool pressure, worker death, session loss — are
  `errors` with `code: "runtime_unavailable"` and `retryable: true`. Exhausting the Lean heap budget is *not* one of
  these: it is a Lean-domain failure, reported in `data` like any other elaboration error.
- **MCP errors** are reserved for I/O and config failures, internal-invariant violations, and unusable Lake projects.

## Prompt-stack verification through MCP

The `check_stack.py` checker used by the KanProofs formalization prompt stacks keeps its Lake/checkdecls backend as the
default. It can also verify through the semantic MCP surface when you start a loopback HTTP server yourself:

```sh
lean-host-mcp --lake-root /path/to/kan-proofs --bind 127.0.0.1:8765

/path/to/check_stack.py /path/to/prompt-workspace \
  --verify \
  --backend mcp \
  --mcp-url http://127.0.0.1:8765/mcp
```

The checker does not start or manage the server. Use a built Lake project, and rebuild/install workers after upgrading
the host so the server and worker protocol stay in step. The MCP backend calls `lean_verify` with sorry rejection and
axiom reporting, using `lean_trial(kind = "command")` only for declarations that verification cannot row-report, such as
definitions whose axiom set must still be checked. `--changed REF --backend mcp` preserves the checker's file-level
changed-prompt selection and uses MCP verification for the selected prompts.

Fallback is explicit:

```sh
/path/to/check_stack.py /path/to/prompt-workspace \
  --verify \
  --backend mcp \
  --mcp-url http://127.0.0.1:8765/mcp \
  --fallback lake
```

Without `--fallback lake`, connection failures, missing tools, or runtime-unavailable setup failures are script errors.
With fallback, the checker reports the fallback and uses the Lake backend for the affected run.

### Internal runtime facts

The operation layer still computes freshness/import and runtime facts before semantic response adaptation. Runtime facts
include worker generation, whether a call observed or performed a restart, retry count, controller queue wait, RSS when
available, import profile, profile-switch count, and restart history. These remain telemetry and are omitted at the
default quiet verbosity. Proof-relevant artifact facts are public under `trust.artifacts` and survive the quiet
telemetry gate: source snapshots (`source` / `file` / `edit_fresh`), build artifacts (`olean` or `ilean` with
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

The harness deliberately does not claim speedups. Keep its JSONL output with any performance change so later comparisons
use the same workload, byte accounting, and cold/warm worker behaviour.
