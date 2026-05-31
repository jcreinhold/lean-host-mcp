# Architecture

`lean-host-mcp` is a thin MCP server over `lean-rs-worker`. The public API is intentionally declaration-centric: agents
name a declaration and, when needed, a proof position inside that declaration. They do not pass cursor coordinates,
source spans, byte offsets, replacement ranges, or inserted spans.

The model-facing tools are:

```text
proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration
```

`find_references` is the bounded semantic support tool. Text grep, Mathlib placement policy, raw hover/type queries, and
low-level term/meta primitives are not part of the public MCP surface.

## Crate Layout

```
crates/lean-host-mcp/src/
  main.rs           clap CLI, shared broker setup, rmcp stdio entry
  transport_http.rs private axum/rmcp Streamable HTTP entry
  server.rs         LeanHostService tool registration
  broker.rs         ProjectBroker: LRU pool, idle eviction, per-call routing
  project.rs        LeanProject worker actor for one Lake project
  tools/            declaration.rs, proof_search.rs, proof_action.rs, position.rs
  projections.rs    stable MCP projections from lean-rs worker types
  lake_meta.rs      minimal Lake-project metadata
  cache.rs          small LRU for bounded reference queries
  envelope.rs       Response<T> = { status, result, freshness, runtime, warnings, next_actions }

crates/lean-host-mcp-worker/src/
  main.rs           worker child: lean_rs_worker_child::run_worker_child_stdio()
```

`server.rs` is glue only. Tool implementations resolve a project through `ProjectBroker`, read source files when needed,
and send one typed read-only semantic job to the project worker actor.

## Transport Boundary

The transport seam is above `LeanHostService`. Stdio and Streamable HTTP both construct the same service over the same
`ProjectBroker`; neither transport owns Lean sessions, project caches, admission policy, restart policy, or tool
schemas. `transport_http.rs` contains the axum and rmcp Streamable HTTP session wiring so HTTP request types do not
reach the tool, broker, or project-runtime layers.

Stdio remains the default process model: one client launches one server and EOF ends the process. `--bind` selects a
single Streamable HTTP server for that run. HTTP sessions are independent rmcp sessions, but the cloned
`LeanHostService` values share one broker, so all sessions still obey the same per-project FIFO actor, process-wide
semantic permit gate, bounded mailbox, RSS policy, and worker restart semantics.

Runtime pressure is not a transport error. Admission pressure, mailbox pressure, worker death, session loss, and restart
recovery continue to surface through the normal tool response envelope as `status = "runtime_unavailable"` with
structured runtime metadata. HTTP status codes are reserved for transport/protocol failures such as invalid headers or
bad HTTP paths.

## Project And Worker Boundary

`ProjectBroker` owns an LRU pool of `Arc<LeanProject>`, keyed by canonical Lake root. It handles idle eviction, manifest
invalidation, multi-project routing, and coalescing concurrent opens for the same canonical root. `LeanProject` owns the
supervised `lean-rs-worker` child and a dedicated actor thread; that thread is the sole owner of the worker handle, so
"exactly one owner of the Lean runtime at a time" is a structural fact rather than a lock discipline. It services one
semantic job at a time in FIFO order and tracks a worker generation counter, the last restart reason, and the last
import profile. A bounded mailbox turns overload into a retryable `busy` failure instead of unbounded memory growth. The
parent binary never links `libleanshared`; the per-toolchain worker child does.

Each project actor runs one semantic job at a time in FIFO mailbox order. The broker also owns a process-wide semantic
permit gate, defaulting to one permit, so cross-project heavy calls are serialized unless the deployment explicitly
raises `LEAN_HOST_MCP_SEMANTIC_PERMITS`. A full project mailbox is a structured retryable infrastructure error rather
than unbounded memory growth.

The actor samples worker RSS before import-profile switches. If the worker is above the configured soft threshold, it
cycles the child before opening the next session. The supervisor also enforces a parent-side hard RSS watchdog during
long-running requests. Fatal child exits are caught inside the actor: the worker is rebuilt, the generation is bumped,
and read-only semantic jobs are retried once. Responses carry runtime facts (`worker_generation`, `worker_restarted`,
`retry_count`, `admission_wait_millis`, `queue_wait_millis`, `call_restart`, `last_restart`) so clients can distinguish
Lean-domain results from infrastructure recovery and lifecycle history.

Each semantic tool opens a short-lived worker session with imports derived from the source header or explicit request.
Lean-domain failures such as parse errors, elaboration diagnostics, missing imports, unsupported shim exports, failed
proof snippets, and sorry policy failures are structured tool results. Recoverable runtime failures are
`runtime_unavailable` tool responses. MCP errors are reserved for invalid requests, I/O/config failures, internal
invariants, and unusable Lake projects.

### Toolchain-Readiness Gate

Before the broker spawns a child for a newly-resolved project, `WorkerBinary::resolve_ready_for` (in `toolchain.rs`)
folds every toolchain-version-drift situation into one `Readiness` verdict. It hides four independently-volatile
decisions behind a single call: the supported window (read directly from `lean_toolchain::SUPPORTED_TOOLCHAINS` — the
host never duplicates the list), the elan layout, the worker install layout, and the header-digest provenance check. The
verdicts:

- `Unsupported` — a numbered pin outside `floor ..= head`. Short-circuits before any filesystem probe (the bogus
  toolchain is usually not installed), so the caller gets the window message, not a buried "elan not installed" error.
- `Stale` — the toolchain's `lean.h` no longer matches the digest recorded when the worker was built (an rc republished
  under the same id, a rebuilt toolchain). Caught before spawn with a rebuild command, instead of the cryptic runtime
  `"incompatible header"` olean crash.
- `NotInstalled` / `ToolchainNotInstalled` — the worker binary, or the elan toolchain itself, is absent.
- `UnknownPin` — a `nightly-*` or otherwise non-`vX.Y.Z` pin: allowed, but the host cannot vouch for it.
- `Ready` — spawn, optionally carrying a soft `note` (e.g. a worker installed by an older host with no provenance
  record).

`project.rs` maps the hard verdicts to one typed `ServerError::BadProject` sentence carrying the corrective command;
`UnknownPin` and the soft `Ready` note ride along as project-lifetime advisories that `LeanProject::freshness` attaches
and `server::wrap` drains into the top-level envelope `warnings`. `install-worker` consults only the pure
`ToolchainId::window_verdict` *before* its multi-minute build, refusing an out-of-window pin and warning on an unknown
one. The digest is hashed once on the cold open/resolve path; the warm broker-reuse path (manifest-hash + health check)
never re-hashes.

### Worker Provenance Sidecar

`install-worker` writes a private `worker.json` next to each installed binary recording the toolchain id, the full
`lean.h` SHA-256 the worker was built against, the `lean_toolchain::LEAN_VERSION` of the build, and whether that digest
matched the supported window at build time. The readiness gate re-hashes the toolchain's current `lean.h` and compares;
a mismatch is `Stale`, a missing sidecar (older host) degrades to a soft warning rather than an error.
`install-worker --list` surfaces both axes per worker: a `support` column (`supported` / `outside-window` / `unknown`)
and a `header` column (`ok` / `stale` / `no-record`).

## Declaration-Centric Proof API

Proof tools share the same anchor shape:

- `file`: the source file containing the declaration.
- `declaration`: the selected declaration name.
- `proof_position`: optional selector inside the declaration.

`proof_position` is intent-shaped:

- `{"kind":"default"}` selects the first open tactic state in declaration order.
- `{"kind":"index","index":N}` selects the Nth open tactic state.
- `{"kind":"after_text","text":"...","occurrence":N}` selects a tactic/source fragment inside the declaration body.

The Lean shim resolves these selectors to private source evidence. Raw offsets, line offsets, syntax spans, indentation,
overlay insertion, and diagnostic-locality classification stay below the worker boundary.

## Tool Semantics

`proof_state` returns the current goals, locals, expected type, diagnostics, cache status, and timing facts for one
declaration proof position.

`search_for_proof` builds a small target profile from `proof_state` or explicit goal/type text, calls bounded lean-rs
declaration search, and ranks metadata-only candidates. It does not render candidate types and does not build a host
declaration index.

`inspect_declaration` inspects exactly one declaration by name. Optional `file` input is used only to derive local
imports so project declarations can resolve. Rendered fields are capped before crossing the worker boundary and carry
their own truncation flags.

`try_proof_step` tries tactic fragments at the selected proof position in an in-memory overlay. It reports per-candidate
status, local diagnostics, downstream diagnostics, resulting goals, and the resolved declaration/proof-position summary.
It never writes source files.

`verify_declaration` elaborates the in-memory source snapshot and checks one declaration under the requested sorry/axiom
policy. Policy failures are normal results.

`find_references` runs semantic reference lookup for a fully-qualified name in file or bounded project scope. Project
scope reports scanned/skipped files, header failures, missing imports, unsupported files, and truncation.

## Removed Public Surface

The server deliberately does not expose:

- low-level term/meta tools: `elaborate`, `kernel_check`, `infer_type`, `whnf`, `is_def_eq`;
- LSP-shaped declaration tools: `hover_by_name`, `type_of_name`, raw `search_declarations`;
- raw module-query access: `lean_query`;
- text search and placement policy: `source_search`, `project_scan`, `mathlib_placement`;
- split reference aliases: `references_in_file`, `references_in_project`.

If one of these capabilities is needed to implement a deeper tool, keep it private and compose it inside the host or
worker. A need to expose it directly is treated as evidence that a better proof-work abstraction is missing.

## Budgets And Caching

Worker APIs enforce per-field and total output budgets before IPC. The host adds hard caps for candidate lists,
reference fanout, and response shape. The worker owns module snapshot reuse and reports cache status, output bytes, and
phase timings for declaration-context queries. The host keeps only a small content-hash LRU for the older single-file
reference path.

Frame size is controlled by query shape, not transport tuning: no public tool requests a whole info tree, bulk rendered
types, or unbounded source/project scans.
