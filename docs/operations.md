# Operations

Operational reference for `lean-host-mcp`: tuning knobs, transport internals, the full runtime-error contract, and the
test and performance harness. Most users need none of this — start with the [README](../README.md) and the
[tool catalog](tool-catalog.md). Reach for this page when you are sizing a deployment, debugging a `runtime_unavailable`
response, or working on the server itself.

## Environment variables

RSS thresholds are in KiB, byte caps in bytes; the parenthetical magnitude is for reading, not for setting.

| Variable | Purpose | Default |
| --- | --- | --- |
| `LEAN_HOST_MCP_PROJECT` | Default Lake root for calls without a `project=` argument. | unset |
| `LEAN_HOST_MCP_BIND` | Loopback `ADDR:PORT` for Streamable HTTP; stdio when unset. | unset |
| `LEAN_HOST_MCP_HTTP_PATH` | Streamable HTTP route. Requires `--bind` / `LEAN_HOST_MCP_BIND`. | `/mcp` |
| `LEAN_HOST_MCP_MAX_PROJECTS` | Resident project **slots**; oldest idle one is evicted on overflow. Counts every open project, including one whose worker is crash-looping (process-dead): it keeps its slot until the next call evicts it (health check) or the idle reaper reclaims it. | `4` |
| `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` | Idle window before a project is reaped. `0` disables. | `600` |
| `LEAN_HOST_MCP_SEMANTIC_PERMITS` | Process-wide permits for heavy semantic work; `1` serializes cross-project calls. | `1` |
| `LEAN_HOST_MCP_SEMANTIC_WAITERS` | Callers that may queue for admission; overflow returns retryable `semantic_admission_full`. | `16` |
| `LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS` | Max admission wait; timeout returns retryable `semantic_admission_timeout`. | `60000` |
| `LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY` | Per-project job mailbox depth; a full mailbox returns retryable `busy`. | `8` |
| `LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB` | Post-job RSS ceiling that triggers a planned worker cycle. | `5242880` (5 GiB) |
| `LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB` | RSS ceiling that cycles a worker before an import-profile switch. | `2097152` (2 GiB) |
| `LEAN_HOST_MCP_WORKER_RSS_HARD_KILL_KIB` | In-flight hard kill; crossing it restarts the child, returning `rss_hard_limit_exceeded`. | `16777216` (16 GiB) |
| `LEAN_HOST_MCP_WORKER_RSS_SAMPLE_MILLIS` | Sampling interval for the in-flight hard-RSS watchdog. | `250` |
| `LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB` | Worker module-snapshot cache RSS guard. | `2097152` (2 GiB) |
| `LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES` | Worker module-snapshot cache byte cap. | `33554432` (32 MiB) |

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
