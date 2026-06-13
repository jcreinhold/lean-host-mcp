# Architecture

> This is a contributor and maintainer document: how the server is built and why. If you want to *use* `lean-host-mcp`,
> start with the [README](../README.md) and the [tool catalog](tool-catalog.md).

`lean-host-mcp` is a thin MCP server over `lean-rs-worker`. The design thesis is a **declaration-centric** public API:
agents name a declaration and, when needed, a proof position inside it — never cursor coordinates, source spans, byte
offsets, or replacement ranges. The host translates those names into bounded, read-only semantic jobs against a
supervised worker child, and projects the worker's results into one stable semantic response shape.

The model-facing tools are:

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

`lean_lookup` owns declaration inspection, proof search, and semantic references. Text grep, Mathlib placement policy,
raw hover/type queries, and low-level term/meta primitives are not part of the public MCP surface.

## Crate Layout

```
crates/lean-host-mcp/src/
  main.rs           clap CLI, shared broker setup, rmcp stdio entry
  transport_http.rs private axum/rmcp Streamable HTTP entry
  server.rs         LeanHostService tool registration
  broker.rs         ProjectBroker: LRU pool, idle eviction, per-call routing
  project.rs        LeanProject serialized controller for one Lake project
  tools/            semantic.rs facade; declaration/proof_search/proof_action/position internals
  projections.rs    stable MCP projections from lean-rs worker types
  lake_meta.rs      minimal Lake-project metadata
  cache.rs          small LRU for bounded reference queries
  envelope.rs       internal operation envelope adapted to { data, errors, trust } at the public boundary

crates/lean-host-mcp-worker/src/
  main.rs           worker child: lean_rs_worker_child::run_worker_child_stdio()
```

`server.rs` is glue only. Tool implementations resolve a project through `ProjectBroker`, read source files when needed,
and send one typed read-only semantic job to the project controller.

## Transport Boundary

The transport seam is above `LeanHostService`. Stdio and Streamable HTTP both construct the same service over the same
`ProjectBroker`; neither transport owns Lean sessions, project caches, admission policy, restart policy, or tool
schemas. `transport_http.rs` contains the axum and rmcp Streamable HTTP session wiring so HTTP request types do not
reach the tool, broker, or project-runtime layers.

Stdio remains the default process model: one client launches one server and EOF ends the process. `--bind` selects a
single Streamable HTTP server for that run. HTTP sessions are independent rmcp sessions, but the cloned
`LeanHostService` values share one broker, so all sessions still obey the same per-project serialized controller,
process-wide semantic permit gate, bounded project queue, RSS policy, and worker lifecycle presentation.

Runtime pressure is not a transport error. Admission pressure, mailbox pressure, worker death, session loss, and restart
recovery continue to surface through the semantic response `errors` channel with structured runtime metadata. HTTP
status codes are reserved for transport/protocol failures such as invalid headers or bad HTTP paths.

## Project And Worker Boundary

`ProjectBroker` owns an LRU pool of `Arc<LeanProject>`, keyed by canonical Lake root. It handles idle eviction, manifest
invalidation, multi-project routing, and coalescing concurrent opens for the same canonical root. `LeanProject` owns a
dedicated controller thread; that thread is the sole owner of the `lean-rs-worker-parent` host handle, so "exactly one
owner of the Lean runtime at a time" is a structural fact rather than a lock discipline. It submits one semantic job at
a time in FIFO order and tracks host presentation facts such as the current worker generation, the last restart reason,
and the last import profile. A bounded project queue turns overload into a retryable `busy` failure instead of
unbounded memory growth. The parent binary never links `libleanshared`; the per-toolchain worker child does.

The broker also owns a process-wide semantic permit gate, defaulting to one permit, so cross-project heavy calls are
serialized unless the deployment explicitly raises `LEAN_HOST_MCP_SEMANTIC_PERMITS`. The same gate is backed by
per-user advisory lock files, so parallel server processes that share `broker.semantic_lock_dir` share one permit pool.
Any path that can open or run a worker must pass through `admit_project`; degraded responses and pure `.ilean` reads use
`project_identity_without_worker` so they can fill semantic trust identity without spawning. A full project queue is a
structured retryable infrastructure error rather than unbounded memory growth.

The controller samples worker RSS before import-profile switches. If the worker is above the configured soft threshold,
it asks `lean-rs-worker-parent` to cycle the service before opening the next session. The worker supervisor enforces the
parent-side hard RSS watchdog during long-running requests. Fatal child exits are reported by the worker layer; the host
maps those structured outcomes once into MCP runtime facts and retries selected read-only semantic jobs once. Responses
carry runtime facts (`worker_generation`, `worker_restarted`, `retry_count`, `admission_wait_millis`,
`queue_wait_millis`, `call_restart`, `last_restart`) so clients can distinguish Lean-domain results from infrastructure
recovery and lifecycle history.

Each semantic tool opens a short-lived worker session with imports derived from the source header or explicit request.
Lean-domain failures such as parse errors, elaboration diagnostics, missing imports, unsupported shim exports, failed
proof snippets, and sorry policy failures are structured tool data. Recoverable runtime failures are semantic `errors`
with `code: "runtime_unavailable"`. MCP errors are reserved for I/O/config failures, internal invariants, and unusable
Lake projects.

The core capabilities the worker exercises across that boundary are the `lean_rs_host_*` symbols (28 mandatory, 6
optional) that ship inside `lean-rs-host` as a vendored Lake package. The semantic proof-search lane uses the same
zero-consumer-setup shape through the package-owned `lean-semantic-search-runtime` crate. `lean-host-mcp` chooses the
host cache root and toolchain/sysroot, but the runtime crate owns the `LeanSemanticSearch` source payload, provenance,
materialization, and Lake build. Consumer projects never declare, link, or import the semantic-search package.

### Toolchain-Readiness Gate

Before the broker spawns a child for a newly-resolved project, `WorkerBinary::resolve_ready_for` (in `toolchain.rs`)
folds every toolchain-version-drift situation into one `Readiness` verdict. It hides five independently-volatile
decisions behind a single call: the supported window (read directly from `lean_toolchain::SUPPORTED_TOOLCHAINS` — the
host never duplicates the list), the elan layout, the worker install layout, the header-digest provenance check, and the
recorded runtime smoke result. The verdicts:

- `Unsupported` — a numbered pin outside `floor ..= head`. Short-circuits before any filesystem probe (the bogus
  toolchain is usually not installed), so the caller gets the window message, not a buried "elan not installed" error.
  The accompanying `nearest` is the genuinely closest supported version (smallest component-wise version distance, ties
  resolved toward the newer release), not merely the window floor.
- `Stale` — the toolchain's `lean.h` no longer matches the digest recorded when the worker was built (an rc republished
  under the same id, a rebuilt toolchain). Caught before spawn with a rebuild command, instead of the cryptic runtime
  `"incompatible header"` olean crash.
- `Unusable` — the worker built and its header digest matches, but it failed its post-build runtime smoke test: the
  toolchain's `libleanshared` is ABI-incompatible with this lean-rs build and crashes when it loads Lean. A matching
  header digest is necessary but **not** sufficient for ABI compatibility, so the recorded smoke result is the sound
  signal. Caught before spawn, instead of a per-call `runtime_unavailable` SIGSEGV loop.
- `NotInstalled` / `ToolchainNotInstalled` — the worker binary, or the elan toolchain itself, is absent.
- `UnknownPin` — a `nightly-*` or otherwise non-`vX.Y.Z` pin: allowed, but the host cannot vouch for it.
- `Ready` — spawn, optionally carrying a soft `note` (e.g. a worker installed by an older host with no provenance
  record, or with a sidecar but no smoke record).

`project.rs` maps the hard verdicts to one typed `ServerError::BadProject` sentence carrying the corrective command;
`UnknownPin` and the soft `Ready` note ride along as project-lifetime advisories that `LeanProject::freshness` attaches
and the semantic facade drains into warning issues. Those advisories are also carried on `WorkerUnavailable`, so a worker
that dies mid-call surfaces them in `errors` rather than dropping them exactly when a suspect worker is most worth
flagging. `install-worker` consults only the pure
`ToolchainId::window_verdict` *before* its multi-minute build, refusing an out-of-window pin and warning on an unknown
one. The digest is hashed once on the cold open/resolve path; the warm broker-reuse path (manifest-hash + health check)
never re-hashes.

The hard verdicts are pre-spawn JSON-RPC `BadProject` errors; project-discovery checks fire first, in order: lakefile
presence → `lake-manifest.json` → window/readiness gate → elan/worker presence. (A directory with no lakefile is
rejected as "not a Lake project" before the window gate runs, so exercising `Unsupported` needs a real pinned Lake
project, not an arbitrary directory.)

### Worker Provenance Sidecar

`install-worker` writes a private `worker.json` next to each installed binary recording the toolchain id, the full
`lean.h` SHA-256 the worker was built against, the `lean_toolchain::LEAN_VERSION` of the build, whether that digest
matched the supported window at build time, and the post-build **smoke** outcome. The smoke test spawns the
freshly-built worker once and runs the cheapest faithful exercise of the FFI boundary — open a session importing `Init`
and inspect `Nat.add_zero` — so an ABI-incompatible worker is caught at install (over the multi-minute build) rather
than at every call. A smoke failure records `smoke: failed` and makes `install-worker` exit non-zero; the binary stays
installed (so `--list` shows it and the gate refuses it with a precise reason) rather than being silently removed. The
readiness gate re-hashes the toolchain's current `lean.h` and compares (a mismatch is `Stale`), then consults the smoke
record (a failure is `Unusable`); a missing sidecar or a sidecar without a smoke record (older host) degrades to a soft
warning rather than an error. `install-worker --list` surfaces three axes per worker: a `support` column (`supported` /
`unsupported` / `unknown`), a `build` column (header-drift — `fresh` / `stale` / `unknown`), and a `runtime` column (the
recorded smoke result — `runs` / `crashed` / `untested`).

## Declaration-Centric Proof API

Proof tools share the same anchor shape:

- `file`: the source file containing the declaration.
- `declaration`: the selected declaration name.
- `proof_position`: optional selector inside the declaration.

`proof_position` is intent-shaped:

- `{"kind":"default"}` (or omitting the field) selects the pristine entry goal — the state before any tactic runs;
  `lean_trial(kind = "proof_step")` splices before the first tactic there.
- `{"kind":"index","index":N}` selects the state after the Nth tactic (`index:0` = after the first tactic).
- `{"kind":"after_text","text":"...","occurrence":N}` selects a tactic/source fragment inside the declaration body.

The Lean shim resolves these selectors to private source evidence. Raw offsets, line offsets, syntax spans, indentation,
overlay insertion, and diagnostic-locality classification stay below the worker boundary.

## Tool Semantics

`tools::semantic` is the public facade. It exposes five MCP tools and routes each `kind` mode to the existing typed
operation modules:

- `lean_context(kind = "proof_position")` returns the current goals, locals, expected type, diagnostics, cache status,
  and timing facts for one declaration proof position.
- `lean_trial(kind = "proof_step")` tries tactic fragments at the selected proof position in an in-memory overlay. It
  reports per-candidate status, diagnostics, resulting goals, and the resolved declaration/proof-position summary. It
  never writes source files.
- `lean_verify` elaborates in-memory source snapshots and checks explicit, file-wide, module-wide, or changed
  declaration target groups under the requested sorry/axiom policy. Policy failures are normal results.
- `lean_lookup(kind = "declaration")` inspects one declaration by name. Optional `file` input derives local imports so
  project declarations can resolve. Rendered fields are capped before crossing the worker boundary.
- `lean_lookup(kind = "declarations")` lists declarations for a file or module. Source files use the edit-fresh worker
  declaration-outline selector; module requests fall back to the existing `.ilean` reader only when the source file is
  unavailable, reporting build-fresh or stale build artifact facts.
- `lean_lookup(kind = "changed_coverage")` maps git hunks to source-fresh declaration spans without verifying them,
  preserving unknown, deleted, and renamed coverage gaps.
- `lean_lookup(kind = "proof_search")` builds a small target profile from proof context or explicit goal/type text, then
  tries a private source-backed `lean-semantic-search` lane before falling back to bounded lean-rs declaration search.
- `lean_lookup(kind = "references")` runs semantic reference lookup for a fully-qualified name in file or bounded project
  scope. File scope elaborates one anchor file through the worker; project scope reads Lean's on-disk `.ilean` reference
  index and does not open a worker.
- `lean_status(kind = "project")` reports cheap project/toolchain/config status from Lake metadata and broker config. It
  accepts `include: ["toolchain", "worker", "artifacts"]`, derives only cheap filesystem facts, and does not run `lake`,
  open a worker, or consume a semantic permit.

The semantic facade also owns the public trust block. Operation modules may attach typed artifact rows for
proof-relevant freshness: source snapshots (`source` / `file` / `edit_fresh`), built Lean artifacts (`olean` or `ilean`
with `build_fresh`, `stale_build`, or `missing_build`), and worker/toolchain availability (`worker` / `toolchain` /
`unknown` or `not_applicable`). Runtime counters, cache timings, and import lists remain telemetry; quiet mode drops
telemetry but never drops trust artifacts.

The rejected alternative was a single GraphQL-like `lean_query` tool with nested selections. It would make tool
registration smaller, but it pushes mode discovery and validation into one large request grammar. The five-tool facade is
clearer for agents: each tool name is a job family, and `kind` selects one stable mode inside that family.

The internal operation names remain in source because they describe narrower implementation jobs. They are not public MCP
registrations.

## Scope Boundary

The surface stays declaration-centric on purpose: every public tool answers a proof-work question about a named
declaration, and lower-level Lean primitives stay private. Capabilities deliberately kept below the boundary, and the
proof-work tool that subsumes each, include:

- low-level term/meta operations (`elaborate`, `kernel_check`, `infer_type`, `whnf`, `is_def_eq`) — composed inside
  `lean_trial(kind = "proof_step")` and `lean_verify`;
- LSP-shaped declaration queries (`hover_by_name`, `type_of_name`, raw `search_declarations`) — folded into
  `lean_lookup(kind = "declaration")` and `lean_lookup(kind = "proof_search")`;
- raw module-query access (`lean_query`) — driven internally by `lean_context(kind = "proof_position")`;
- text search and placement policy (`source_search`, `project_scan`, `mathlib_placement`);
- split reference aliases (`references_in_file`, `references_in_project`) — unified under
  `lean_lookup(kind = "references")`.

When one of these is needed to implement a deeper tool, keep it private and compose it inside the host or worker. A need
to expose it directly is treated as evidence that a better proof-work abstraction is missing.

## Budgets And Caching

Worker APIs enforce per-field and total output budgets before IPC. The host adds hard caps for candidate lists,
reference fanout, and response shape. The worker owns module snapshot reuse and reports cache status, output bytes, and
phase timings for declaration-context queries. The host keeps only a small content-hash LRU for the older single-file
reference path.

Frame size is controlled by query shape, not transport tuning: no public tool requests a whole info tree, bulk rendered
types, or unbounded source/project scans.
