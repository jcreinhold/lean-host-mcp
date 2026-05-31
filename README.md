# lean-host-mcp

A Model Context Protocol server that hosts Lean 4 directly: it runs the elaborator and kernel inside a supervised worker
child and reaches them as in-process calls, not as messages to an external LSP. A wedged tactic or runaway typeclass
loop kills the child, and the supervisor restarts it instead of taking down the server. That is the difference from
`lean-lsp-mcp`.

The split has two crates. The worker child (`lean-rs-worker-child`) owns the `LeanRuntime` and the bundled host-shim
capabilities; the parent (`lean-rs-worker-parent`) owns a shims-only `LeanWorkerHostHandle` and does **not** link
`libleanshared`. Keeping the parent free of the Lean dylib lets one running `lean-host-mcp` serve projects on different
Lean toolchains—each has its own pre-built worker binary under `~/.local/share/lean-host-mcp/workers/<toolchain>/`.

The public tool surface is the proof workflow, not a mirror of Lean's runtime internals: `proof_state`,
`search_for_proof`, `inspect_declaration`, `try_proof_step`, `verify_declaration`, and `find_references`. Per-tool
request and result schemas live in [`docs/tool-catalog.md`](docs/tool-catalog.md); internal layering in
[`docs/architecture.md`](docs/architecture.md).

## Prerequisite: any built Lake project

A consumer project needs only:

- A `lakefile.lean` or `lakefile.toml`.
- A successful `lake build` for the modules the tools will import, so their `.olean` files exist on the search path. The
  default `lake build` with no target is the usual setup step.

The `lean_rs_host_*` symbols the worker needs (28 mandatory, 6 optional) ship inside `lean-rs-host` as a vendored Lake
package. The host builds it once per toolchain at first session open and loads it without touching the consumer
project's `:shared` facet, so consumer projects never declare, link, or `@[export]` it.

Dependencies need no extra configuration once their own `lake build` has run. The server reads `lake-manifest.json`,
walks each transitive package's `.lake/packages/<name>/.lake/build/lib/lean`, and adds those directories to the import
search path. For mathlib, `lake exe cache get` pulls precompiled oleans; other dependencies follow the equivalent setup.

`fixtures/lean/` is the demo target the test suite uses. It also doubles as a minimal template—copy it and adapt to
taste.

## Build and run

```sh
# 1. Install the parent binary. Build per-member, never `cargo build --workspace`
#    (workspace builds unify feature flags and silently link libleanshared
#    into the parent).
cd /path/to/lean-host-mcp
cargo install --path crates/lean-host-mcp

# 2. Install worker binaries for your local Lean toolchains.
#    With no mode flag, install-worker scans ~/.elan/toolchains and builds
#    any missing workers; each target lands under
#    ~/.local/share/lean-host-mcp/workers/<id>/.
lean-host-mcp install-worker
lean-host-mcp install-worker --toolchain v4.30.0
lean-host-mcp install-worker --list           # see what's installed

# 3a. Zero-config: launch from inside (or anywhere under) any built Lake
#     project. The toolchain pin is read from `lean-toolchain`, the project
#     root from `lakefile.{lean,toml}`. Tool calls own their own `imports`;
#     no project umbrella is imported unless a call passes it explicitly.
cd /path/to/your/lake/project
lake build && lean-host-mcp

# 3b. Explicit: pin the default project. Equivalent to setting
#     LEAN_HOST_MCP_PROJECT.
lean-host-mcp --lake-root /path/to/your/lake/project
```

## Transports

`lean-host-mcp` serves exactly one transport per process.

Stdio is the default and is the right choice for clients that launch an MCP server with a `command`:

```sh
lean-host-mcp --lake-root /path/to/your/lake/project
```

Streamable HTTP is selected by `--bind` or `LEAN_HOST_MCP_BIND`:

```sh
lean-host-mcp serve --lake-root /path/to/your/lake/project --bind 127.0.0.1:8765
```

The default HTTP route is `/mcp`; override it with `--http-path /some-path` or `LEAN_HOST_MCP_HTTP_PATH`. `--http-path`
requires `--bind`; it never switches transports by itself. HTTP binds are loopback-only for now (`127.0.0.1` or `::1`).
The server has no built-in authentication or TLS, so non-loopback addresses are rejected rather than merely discouraged.

Example Streamable HTTP client configuration for clients that accept a URL:

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "url": "http://127.0.0.1:8765/mcp"
    }
  }
}
```

Project resolution chain (used by every tool call that does not pass its own `project="..."` argument):

1. `LEAN_HOST_MCP_PROJECT` (or `--lake-root`)
2. Walk upward from the server's cwd looking for `lakefile.{toml,lean}`
3. `~/.config/lean-host-mcp/config.toml` `primary_project = "/abs/path"`

Per call, an MCP client can pass `project="/abs/path/to/other/lake/root"` to route that single call elsewhere—useful
when a single client surveys several projects.

Environment vars:

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

### Process lifetime

The idle reaper (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`) governs the per-project worker sub-actors, not the parent server
process. A stdio server exits when its transport closes: it serves until the client closes the server's stdin (the
normal disconnect path), so a well-behaved launcher reaps it automatically. Orphaned `lean-host-mcp` parents left
running are launcher artifacts (a client that spawned a server and never closed its stdin), not leaked sessions — the
server holds no project resident once its transport is gone.

## Wiring into Claude Code

```jsonc
{
  "mcpServers": {
    "lean-host": {
      "command": "/abs/path/to/lean-host-mcp/target/release/lean-host-mcp"
      // No args needed when the client launches the server inside the
      // target Lake project; otherwise pass `--lake-root /abs/path`.
    }
  }
}
```

**One server, multiple toolchains.** The server picks the worker binary for each project from its `lean-toolchain` pin
and sets `LEAN_SYSROOT` invisibly per spawn (via `LeanWorkerChild::for_toolchain`). A single `lean-host-mcp` process can
serve projects on every toolchain you have installed a worker for (`lean-host-mcp install-worker --toolchain <id>`). You
do not need to set `LEAN_SYSROOT` in the MCP client config.

## Response envelope

Every tool returns the same outer shape; `result` is tool-specific and `runtime_error` is populated only for recoverable
runtime failures.

```jsonc
{
  "status": "ok",                    // or "runtime_unavailable"
  "result":   { /* tool-specific */ },
  "runtime_error": null,              // populated when status is "runtime_unavailable"
  "freshness": {
    "project_root":   "/abs/path",
    "project_hash":   "sha256-hex of lake-manifest.json",
    "imports":        ["Mod.A", "..."],
    "session_id":     "uuid",          // stable identity of the project actor; changes only on re-spawn (LRU/idle/manifest)
    "lean_toolchain": "leanprover/lean4:v4.29.1"
  },
  "runtime": {
    "worker_generation": 1,
    "worker_restarted": false,
    "retry_count": 0,
    "admission_wait_millis": 0,
    "queue_wait_millis": 0,
    "call_restart": null,
    "last_restart": null,
    "rss_kib": null,
    "worker_lanes": 1,
    "import_profile": "Init\nProject.Module",
    "profile_switch_count": 0
  },
  "warnings":     ["..."],     // omitted when empty
  "next_actions": ["..."]      // omitted when empty
}
```

Recoverable runtime/actor failures are normal tool responses, not JSON-RPC errors:

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
  "freshness": { /* same shape as above */ },
  "runtime": { /* best-known runtime facts */ },
  "warnings": [],
  "next_actions": []
}
```

`freshness.imports` is the import vector used for that call: explicit request imports for declaration/proof-search tools
and file-header imports for module-query tools. An empty array means the call used no extra imports beyond the worker's
base environment.

`runtime` is attached to semantic tool calls. It reports the current worker generation, whether this call observed or
performed a restart, retry count, admission wait, actor queue wait, RSS when available, the import profile,
profile-switch count, `call_restart` for this call, and `last_restart` as lifecycle history. Lean-domain failures
(parse, elaboration, kernel rejection, meta timeout) are part of the `ok` payload. Retryable runtime failures such as
admission pressure, mailbox pressure, worker death, or session loss are `runtime_unavailable` responses. MCP errors are
reserved for invalid requests, I/O/config failures, internal invariants, and unusable Lake projects.

## Capability shims and proof-agent module queries

`proof_state` is the common proof-agent call: one request returns a compact context—diagnostics, goals, locals, expected
type, target declaration, and the surrounding declaration. It depends on the optional bounded
`lean_rs_host_process_module_query_batch` shim; a worker whose bundled shims lack that capability answers
`{ "status": "unsupported" }`. No public tool requests or caches whole-file info trees. Successful responses carry
`query_facts` (worker cache status, output bytes, phase timings), and repeated calls reach the worker snapshot cache, so
warm behavior is observable.

Files whose header imports modules the server's open env doesn't have are still processed; missing imports surface as an
envelope warning. Files using Lean 4's module-system header syntax, including `module`, `public import`, `import all`,
and `meta import`, are supported. A header that doesn't parse short-circuits to `header_parse_failed`.

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

## Smoke/perf baseline

The ignored `smoke_perf` integration test is the black-box baseline harness for proof-agent work. It starts the compiled
stdio MCP server, calls `tools/list`, runs representative tool calls, and emits JSONL rows with wall time, serialized
response bytes, 32 KiB / 64 KiB budget flags, status, warning count, observable project-session changes, and process RSS
when the platform exposes it. For `proof_state`, rows also include the worker module-cache status, worker-reported
output bytes, phase timings, and optional worker cache size facts. The budget constants are test-only guardrails:
ordinary model-facing responses should aim for 16-32 KiB, with 64 KiB as the default hard ceiling. Production truncation
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

## Versions

`lean-host-mcp` 0.1.0 targets `lean-rs-worker-parent` / `lean-rs-worker-child` 0.1.17 (which transitively pin `lean-rs`
/ `lean-rs-host` 0.1.17). The supported Lean window is `4.26.0 ..= 4.31.0-rc1` — the head, Lean **4.31.0-rc1**, is the
version this release is built and tested against. The server inherits whichever Lean toolchain each consumer Lake
project pins, provided it sits inside the `lean-rs` support window declared by
[`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). The host reads that window
directly from `lean-toolchain::SUPPORTED_TOOLCHAINS` rather than duplicating it: a project pinning a toolchain outside
the window is rejected at open with a one-line verdict naming the window and the nearest supported version, and
`install-worker` refuses to build for an out-of-window pin. Bumping the supported toolchain is a `lean-rs` change first,
then a version bump here.

## License

MIT OR Apache-2.0. See `LICENSE-MIT`, `LICENSE-APACHE`.
