# Operations

Operational reference for `lean-host-mcp`: tuning knobs, transport internals, the full runtime-error contract, and the
test and performance harness. Most users need none of this — start with the [README](../README.md) and the
[tool catalog](tool-catalog.md). Reach for this page when you are sizing a deployment, debugging a `runtime_unavailable`
response, or working on the server itself.

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
| `runtime.project_mailbox_capacity` | integer | `8` | `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | How many calls may queue for one project's worker before new calls are shed with a retryable busy status. |
| `runtime.worker_restart_limit` | integer | `3` | `LEAN_HOST_MCP_WORKER_RESTART_LIMIT` | How many worker restarts are tolerated within the restart window before the project is marked unhealthy. |
| `runtime.worker_restart_window_secs` | integer (s) | `60` | `LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS` | Rolling window, in seconds, over which worker_restart_limit is counted. |
| `broker.max_projects` | integer | `4` | `LEAN_HOST_MCP_MAX_PROJECTS` | How many distinct Lake projects stay open at once; on overflow the least-recently-used project's worker is evicted. |
| `broker.idle_timeout_secs` | integer (s) | `600` | `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Evict a project's worker after this many idle seconds. 0 disables idle eviction. Default 10 minutes. |
| `broker.semantic_permits` | integer | `1` | `LEAN_HOST_MCP_SEMANTIC_PERMITS` | How many semantic (elaborating) calls run concurrently across all projects. Lean elaboration is single-threaded per worker, so raising this helps only when hosting several projects at once. |
| `broker.semantic_waiters` | integer | `16` | `LEAN_HOST_MCP_SEMANTIC_WAITERS` | How many semantic calls may queue for a permit before new ones are shed with a retryable semantic_admission_full status. |
| `broker.semantic_admission_timeout_millis` | integer (ms) | `60000` | `LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS` | How long a semantic call waits for a permit before giving up with a retryable semantic_admission_timeout status. Default 60 seconds. |
| `server.bind` | string (loopback ADDR:PORT) | unset | `--bind / LEAN_HOST_MCP_BIND` | Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS. |
| `server.http_path` | string | unset | `--http-path / LEAN_HOST_MCP_HTTP_PATH` | HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp. |

<!-- END GENERATED -->

The three RSS ceilings must satisfy `import_switch <= post_job <= hard_kill`; the server **refuses to start** with a
clear `invalid RSS config: …` message otherwise (an inverted order makes the cheaper planned cycle unreachable — e.g.
`post_job` above `hard_kill` means every overrun escalates straight to an in-flight hard kill). To recycle less often
under memory pressure, raise `post_job` toward (but below) `hard_kill` — e.g.
`LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB=8388608` for an 8 GiB post-job ceiling.

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

The idle reaper (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`) governs the per-project worker sub-actors, not the parent server
process. A stdio server exits when its transport closes: it serves until the client closes the server's stdin (the
normal disconnect path), so a well-behaved launcher reaps it automatically. Orphaned `lean-host-mcp` parents left
running are launcher artifacts (a client that spawned a server and never closed its stdin), not leaked sessions — the
server holds no project resident once its transport is gone.

## Runtime-error contract

Every tool returns the same envelope (the `ok` shape is in the [README](../README.md#response-envelope)). Recoverable
runtime and actor failures are normal tool responses with `status: "runtime_unavailable"`, not JSON-RPC errors:

```jsonc
{
  "status": "runtime_unavailable",
  "result": null,
  "runtime_error": {
    "reason": "semantic_admission_timeout",
    "retryable": true,
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
  },
  "freshness": { /* same shape as the ok envelope */ },
  "runtime": { /* best-known runtime facts */ },
  "warnings": [],
  "next_actions": []
}
```

Which failures land where:

- **Lean-domain failures** — parse errors, elaboration diagnostics, kernel rejection, meta timeout — are part of the
  `ok` payload. A failed proof is a successful tool call.
- **Retryable runtime failures** — admission pressure, mailbox pressure (`busy`), worker death, session loss, RSS
  hard-kill — are `runtime_unavailable` responses with `retryable: true`.
- **MCP errors** are reserved for invalid requests, I/O and config failures, internal-invariant violations, and unusable
  Lake projects.

### `freshness` and `runtime` fields

`freshness.imports` is the import vector used for that call: explicit request imports for declaration and proof-search
tools, file-header imports for module-query tools. An empty array means the call used no extra imports beyond the
worker's base environment.

`runtime` is attached to semantic tool calls. It reports the current worker generation, whether this call observed or
performed a restart, retry count, admission wait, actor queue wait, RSS when available, the import profile,
profile-switch count, `call_restart` for this call, and `last_restart` as lifecycle history. These let a client
distinguish a Lean-domain result from infrastructure recovery.

## Capability shims and module queries

`proof_state` is the common proof-agent call: one request returns a compact context — diagnostics, goals, locals,
expected type, target declaration, and the surrounding declaration. It depends on the optional bounded
`lean_rs_host_process_module_query_batch` shim; a worker whose bundled shims lack that capability answers
`{ "status": "unsupported" }`. No public tool requests or caches whole-file info trees. Successful responses carry
`query_facts` (worker cache status, output bytes, phase timings), and repeated calls reach the worker snapshot cache, so
warm behavior is observable.

Files whose header imports modules the server's open env doesn't have are still processed; missing imports surface as an
envelope warning. Files using Lean 4's module-system header syntax — `module`, `public import`, `import all`, and
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
when the platform exposes it. For `proof_state`, rows also include the worker module-cache status, worker-reported
output bytes, phase timings, and optional worker cache size facts. The budget constants are test-only guardrails:
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
