# Architecture

> This is a contributor and maintainer document: how the server is built and why. If you want to *use* `lean-host-mcp`,
> start with the [README](../README.md) and the [tool catalog](tool-catalog.md).

`lean-host-mcp` is a thin MCP server over `lean-rs-worker`. The design thesis is a **declaration-centric** public API:
agents name a declaration and, when needed, a proof position inside it — never cursor coordinates, source spans, byte
offsets, or replacement ranges. The host translates those names into bounded semantic jobs against a supervised worker
child without applying source edits automatically, then projects the results into one stable semantic response shape.
Executed Lean code can perform IO with the server user's privileges; the worker is not an OS sandbox.

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
  broker/control.rs process-local worker activity snapshots and expiring admission leases
  project.rs        LeanProject serialized controller for one Lake project
  tools/            semantic.rs facade; declaration/proof_search/proof_action/position internals
  projections.rs    stable MCP projections from lean-rs worker types
  lake_meta.rs      minimal Lake-project metadata
  cache.rs          content-hashed LRU over every bounded module query
  envelope.rs       internal operation envelope adapted to { data, errors, trust } at the public boundary

crates/lean-host-mcp-worker/src/
  main.rs           worker child: lean_rs_worker_child::run_worker_child_stdio()
```

`server.rs` is glue only. Tool implementations resolve a project through `ProjectBroker`, read source files when needed,
and send one typed semantic job to the project controller.

## Transport Boundary

The transport seam is above `LeanHostService`. Stdio and Streamable HTTP both construct the same service over the same
`ProjectBroker`; neither transport owns Lean sessions, project caches, queueing policy, restart policy, or tool schemas.
`transport_http.rs` contains the axum and rmcp Streamable HTTP session wiring so HTTP request types do not reach the
tool, broker, or project-runtime layers.

Stdio remains the default process model: one client launches one server and EOF ends the process. `--bind` selects a
single Streamable HTTP server for that run. HTTP sessions are independent rmcp sessions, but the cloned
`LeanHostService` values share one broker, so all sessions still obey the same per-project serialized controller,
bounded project queue, memory budget, and worker lifecycle presentation.

MCP 2026-07-28 uses the SDK's stateless discovery and per-request capability/version metadata. Legacy clients retain
initialize/session support. Both paths share the same service and retained-request registry. Protocol guards follow
[the MCP versioning contract](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning). HTTP is
native-only: every present `Origin` is rejected with 403, while requests without Origin still require an allowed
loopback Host. This follows the
[Streamable HTTP security requirements](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http).

Runtime pressure is not a transport error. Mailbox pressure, project-pool pressure, worker death, session loss, and
restart recovery continue to surface through the semantic response `errors` channel with structured runtime metadata.
HTTP status codes are reserved for transport/protocol failures such as invalid headers or bad HTTP paths.

Request cancellation reaches the project queue and the worker supervisor. Cancelled queued jobs are skipped before
opening a Lean session; in-flight cancellation uses the worker layer's token and may recycle that project's worker.
Cancellation remains bounded by supervisor observation and deadlines, rather than being an instantaneous stop. The
owning operation waits for its terminal cleanup instead of treating a cancellation acknowledgement as termination.
Dropping a caller's future also signals cancellation without freeing the worker's capacity early.

`lean_compatibility` uses the same broker admission, actor mailbox, startup accounting, and worker-slot lifecycle as
semantic calls. Its actor job opens a checked host session but submits no target proof. The broker owns one required
host-binding subset per supported compiled-header ABI; loaded actor/generation receipts remain separate from that
expected contract. Worker-session configuration enforces the subset after every recycle, and semantic trust preserves
actual receipts independently of telemetry verbosity. This replaces caller-side binary/shim profiles rather than adding
a second admission queue.

`lean_runtime` is an additional synchronous operational tool, separate from the five semantic job families. Its
idle-pool lease shares the broker registry mutex with both resident project borrowing and startup-lock acquisition. A
claim is refused while any caller holds a project, any startup lock is retained, or any actor-owned message remains
active. Background recycling and prewarming acquire actor activity under that same lock; old controllers cannot start it
after eviction. Pool activity counts survive registry removal and remain held through cleanup. Once claimed, new worker
admission is refused until release or monotonic expiry. Idle reaping also pauses, preserving warm caches unless the
acquirer explicitly requests retirement. Retirement only closes idle actors' mailboxes; the existing worker permits
still wait for confirmed shutdown. Snapshots and shutdown requests alone do not establish that resources have been
freed. The token and broker instance identity are process-local, with no cross-process scheduling, priority policy,
preemption, or persistence. See the [tool catalog](tool-catalog.md#worker-activity-and-admission-leases) for the public
contract.

Cooperating artifact writers announce process-bound demand on the same canonical physical domains before waiting for
exclusive artifact custody. The broker checks that handoff gate before borrowing a resident actor or admitting idle
maintenance. Contention pauses affected new calls within their existing cancellation/deadline budget; admitted calls
finish normally. A separate 100 ms observation in the existing reaper retires only affected idle, unborrowed actors,
even when ordinary idle expiry is disabled or a lease preserves warm caches. It does not advance or release that lease.
Retirement still uses the ordinary process owner, so an acknowledgement never releases capacity or artifact custody
before actual scope closure. Nonoverlapping actors remain available. Matching uses the actor's retained actual read-domain
receipts, including frozen dependencies and companions, rather than a newly reduced manifest. A brief competing reader
gate can also cause conservative backpressure or idle retirement; it is not evidence of a writer identity. The protocol
does not claim FIFO ordering among multiple writers or defeat OS scheduling/filesystem stalls.

The generic `reader` and `read-proxy` admissions still refuse writer demand before Ready. They can be composed across
multiple physical domains, so waiting while retaining an earlier shared lock would create a reader/writer cycle.
A metadata caller holding no other artifact read or write custody may explicitly select
`read-proxy --single-domain-wait`. This one-domain proxy retries only lock admission under its original absolute
admission deadline, closes transient gates before waiting, and observes its private FIFO caller on each iteration and
again before Ready. EOF or deadline expiry emits no Ready. Metadata executes once in the caller after that receipt;
neither a metadata body nor a target check is retried. SDK multi-domain readiness probes retain the fail-fast default.

Whole-domain admission owns the complete fixed input set. Its opted-in pure helpers report normal exit 75 only for
typed pre-custody contention (`--report-contention`); every malformed receipt, descriptor, helper failure or uncertain
cleanup remains a hard error. The SDK verifies successful pre-transfer classification, releases all tentative shared
locks (including confirmed EOF-proxy helper closure), then waits within the original deadline and cancellation budget.
It never waits for a later domain while retaining an earlier partial set, and never repeats a payload or metadata body.
A full-set gate preflight avoids spawning repeated helpers while known writer demand remains; it grants no read
custody, and the actual nonblocking transfer checks close the following race. Native payload bootstrap and cold worker
admission finish the full set before guardian acknowledgement. A warm worker's guardian retains earlier generation
locks, so warm contention uses the existing confirmed pre-body generation cycle instead of waiting behind its own
custody. Graph, epoch, inheritance and guardian failures after admission are not retried.

The supervisor retains a failed replacement process when cleanup cannot confirm its exit. An earlier generation's exit
cannot satisfy structured shutdown for that replacement. Consequently an internal idle recycle failure also keeps the
project's capacity occupied until shutdown confirms cleanup, or conservatively retains it if cleanup remains uncertain.

## Project And Worker Boundary

`ProjectBroker` owns an LRU pool of `Arc<LeanProject>`, keyed by canonical Lake root. It handles idle eviction, manifest
invalidation, multi-project routing, and coalescing concurrent opens for the same canonical root. `LeanProject` owns a
dedicated controller thread; that thread is the sole owner of the `lean-rs-worker-parent` host handle, so "exactly one
owner of the Lean runtime at a time" is a structural fact rather than a lock discipline. It submits one semantic job at
a time in FIFO order and tracks host presentation facts such as the current worker generation, the last restart reason,
and the last import profile. A bounded project mailbox admits further callers in FIFO order; admission waits are
cancellable and deadline-bounded. Cold-project capacity uses a separate FIFO admission queue, so clients do not race
retries for idle workers. The parent binary never links `libleanshared`; the per-toolchain worker child does.

The broker reserves capacity before opening a worker. Opening, resident and retiring project workers all count against
`broker.max_workers`; a slot is released only after confirmed shutdown. Ordinary workers, semantic companions and
replacement generations share an actor-owned process tracker. When the actor stops accepting jobs and its scope ends, a
retirement registry keeps the slot and tracker until every direct child has been reaped. Late exit observation
automatically restores capacity, including after cleanup-thread creation fails and broker polling retries. An
unconfirmed exit keeps the slot unavailable. This registry is process-local; it does not reconcile server crashes or
Lake descendants. Same-project startup is serialized even when a caller disconnects, and rechecks the per-project worker
limit. Idle LRU workers may be replaced when capacity is full, while a worker borrowed by a caller or processing queued
work cannot be evicted. Capacity counts project runtimes rather than memory bytes. It does not cover the transient
semantic-search companion child described below or external Lean processes. Different projects can still run
concurrently. A project can use several workers when configured, with one in-flight job per worker. Degraded responses
and pure `.ilean` reads use `project_identity_without_worker` so they can fill semantic trust identity without spawning,
and a warm module-query cache hit is answered from the project handle without entering the mailbox. A full project queue
is a structured retryable infrastructure error rather than unbounded memory growth.

Worker memory is bounded by retained import bytes, not by watching the process. Above that sits
`runtime.lean_max_memory_kib`, a Lean *heap* ceiling enforced by `lean_internal_set_max_memory` — a backstop rather than
a second policy, because crossing it aborts the child: Lean's allocator signals the overrun with a C++ exception that
cannot unwind through the Rust frame at the shim boundary. Its default is therefore derived from the residue budget
(doubled) rather than fixed, so the clean recycle always fires first. Nothing restarts on resident memory: RSS counts
the shared, clean, mmapped `.olean` pages every Mathlib-scale worker maps at startup, which made the four thresholds
this replaced fire on healthy workers. RSS is still sampled once per call for the `rss_kib` runtime fact; no policy
reads it.

What accumulates instead is *imports*. A session whose imports match the live one is reused, so a child's retained state
no longer scales with call count — but an environment imported with `loadExts := true` is never reclaimed, so it still
scales with what a child has imported. The unit is bytes, not imports: `.olean` compacted regions are position-dependent
and each `importModules` gets a fresh `ImportState`, so only the first import in a process is memory-mapped and every
later one re-materializes its whole closure — including already-mapped modules — as private dirty pages. Measured
against real per-file headers at Mathlib scale, that is 10–18 MB for the first import and 2.0–4.5 GB for each one after
it, and five *near-identical* profiles cost more (16.0 GiB) than five unrelated ones (9.6 GiB) because overlap is what
gets re-copied. The policy restart both worker builders set therefore bounds the residue in the unit the child itself
reports — `max_import_residue_bytes`, accumulated from `LeanWorkerImportStats::non_memory_mapped_region_bytes`, which
predicts physical footprint growth with slope 1.09 and R² 0.96. A `max_imports` backstop sits underneath it, high enough
never to fire except when a child reports zero-valued import statistics.

Because the residue is unreclaimable either way, the child *keeps* the outgoing environment instead of dropping it when
the imports change, in a session pool whose capacity is now an independent knob rather than a mirror of the restart
bound. Returning to a profile it still holds costs a key comparison rather than an import, at a measured ~70 MiB per
extra live environment against the 2.0–4.5 GB an import costs. A workload that repeats one profile never trips the
bound, and neither does one alternating among as many profiles as the pool holds; a sweep across more distinct profiles
than the budget covers recycles, which is physics rather than policy. Cycles that do fire are planned, appear as
`import_residue` in `call_restart`, and do not consume the abnormal-restart budget — and when the project actor finds
itself idle above 60% of the budget it takes the same cycle early as `import_residue_idle`, re-importing the most recent
profile so the spawn and the import both land between calls. Fatal child exits are reported by the worker layer; the
host maps those structured outcomes once into MCP runtime facts. It does not automatically replay interrupted jobs: Lean
elaboration and tactics can perform IO, so worker death does not establish replay safety. Responses carry runtime facts
(`worker_generation`, `worker_restarted`, `retry_count`, `queue_wait_millis`, `call_elapsed_millis`, `call_cpu_millis`,
`call_restart`, `last_restart`, `final_attempt`, `previous_attempt_failure`) so clients can distinguish Lean-domain
results from infrastructure recovery and lifecycle history. The two timing fields bracket the call: `queue_wait_millis`
covers admission delay, `call_elapsed_millis` the worker-call wallclock (queue excluded), and `call_cpu_millis` the
worker child's own CPU delta — contention-immune, and the figure the `output.slow_call_warning_millis` (default 10 s)
per-call refactoring warning keys on. The server reports these facts and the client decides policy; the single exception
is opt-in: `retry_tainted_non_positive` on trial/verify requests asks the server to apply one specific policy itself —
re-issuing a non-positive verdict once when the runtime was recycled mid-call — and the retry is still surfaced through
the same `retry_count` fact rather than hidden.

The controller keeps an earlier failed attempt separate from the final attempt. A completed retry returns its result
with a warning naming the earlier failure. A restart during the final attempt still invalidates the result; an unknown
or interrupted retry never becomes cacheable merely because the retry started. The warning remains visible when runtime
telemetry is off.

Each semantic tool opens a worker session with imports derived from the source header or explicit request. The worker
child answers a matching open from its live session, or from one it has pooled, rather than importing again — so a
session outlives the call that opened it and survives calls under other import profiles. Each such environment is a
snapshot taken at *its own* import, so a child holding several holds several snapshots of differing age. The controller
mirrors the pool: it stamps the newest `.olean` mtime among the incoming profile's imports before each call, against the
stamp it recorded the last time that same profile ran, and cycles the worker when the stamp advances —
`artifacts_rebuilt` in `runtime.call_restart`. That recycle is pre-job, so the call is still answered by a live worker.
Lean-domain failures such as parse errors, elaboration diagnostics, missing imports, unsupported shim exports, failed
proof snippets, and sorry policy failures are structured tool data. Recoverable runtime failures are semantic `errors`
with `code: "runtime_unavailable"`. MCP errors are reserved for I/O/config failures, internal invariants, and unusable
Lake projects.

The core capabilities the worker exercises across that boundary are the `lean_rs_host_*` symbols (34 mandatory, 13
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
and the semantic facade drains into warning issues. Those advisories are also carried on `WorkerUnavailable`, so a
worker that dies mid-call surfaces them in `errors` rather than dropping them exactly when a suspect worker is most
worth flagging. `install-worker` consults only the pure `ToolchainId::window_verdict` *before* its multi-minute build,
refusing an out-of-window pin and warning on an unknown one. The digest is hashed once on the cold open/resolve path;
the warm broker-reuse path (manifest-hash + health check) never re-hashes.

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
  `lean_trial(kind = "proof_step")` splices tactic text before the first tactic there.
- `{"kind":"index","index":N}` selects the state after the Nth tactic (`index:0` = after the first tactic).
- `{"kind":"after_text","text":"...","occurrence":N}` selects a worker-recognized proof-state boundary matching a
  tactic/source fragment inside the declaration body. It is not an arbitrary substring cursor.

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
- `lean_trial(kind = "command")` runs bounded Lean command text such as `#check` or `#print axioms`, either under
  explicit imports or after a current file source snapshot. Info messages become rendered output; warnings and errors
  stay in diagnostics.
- `lean_verify` elaborates in-memory source snapshots and checks explicit, file-wide, or module-wide declaration target
  groups under the requested sorry/axiom policy. Policy failures are normal results. All MCP handlers wrap the operation
  with token-gated progress notifications, including 15-second heartbeats, so transport visibility remains observational
  and does not enter the job or worker layers.
- `lean_lookup(kind = "declaration")` inspects one declaration by name. Optional `file` input derives local imports so
  project declarations can resolve. Rendered fields are capped before crossing the worker boundary.
- `lean_lookup(kind = "declarations")` lists declarations for a file or module. Source files use the edit-fresh worker
  declaration-outline selector; module requests fall back to the existing `.ilean` reader only when the source file is
  unavailable, reporting build-fresh or stale build artifact facts.
- `lean_lookup(kind = "proof_search")` builds a small target profile from proof context or explicit goal/type text, then
  tries a private source-backed `lean-semantic-search` lane before falling back to bounded lean-rs declaration search.
- `lean_lookup(kind = "references")` runs semantic reference lookup for a fully-qualified name in file or bounded
  project scope. File scope elaborates one anchor file through the worker; project scope reads Lean's on-disk `.ilean`
  reference index and does not open a worker.
- `lean_status(kind = "project")` reports cheap project/toolchain/config status from Lake metadata and broker config. It
  accepts `include: ["toolchain", "worker", "artifacts"]`, derives only cheap filesystem facts, and does not run `lake`
  or open a worker.
- `lean_status(kind = "file_diagnostics")` elaborates the current source snapshot for one file and returns bounded
  diagnostics with edit-fresh source trust. It is not a replacement for `lake build`, `lake exe lint`, or project
  linters.

The semantic facade also owns the public trust block. Operation modules may attach typed artifact rows for
proof-relevant freshness: source snapshots (`source` / `file` / `edit_fresh`), built Lean artifacts (`olean` or `ilean`
with `build_fresh`, `stale_build`, or `missing_build`), and worker/toolchain availability (`worker` / `toolchain` /
`unknown` or `not_applicable`). Runtime counters, cache timings, and import lists remain telemetry; quiet mode drops
telemetry but never drops trust artifacts.

The rejected alternative was a single GraphQL-like `lean_query` tool with nested selections. It would make tool
registration smaller, but it pushes mode discovery and validation into one large request grammar. The five-tool facade
is clearer for agents: each tool name is a job family, and `kind` selects one stable mode inside that family.

The internal operation names remain in source because they describe narrower implementation jobs. They are not public
MCP registrations.

## Scope Boundary

The surface stays declaration-centric on purpose: every public tool answers a proof-work question about a named
declaration, and lower-level Lean primitives stay private. Capabilities deliberately kept below the boundary, and the
proof-work tool that subsumes each, include:

- low-level term/meta operations (`elaborate`, `kernel_check`, `infer_type`, `whnf`, `is_def_eq`) — composed inside
  `lean_trial(kind = "proof_step")`, `lean_trial(kind = "command")`, and `lean_verify`;
- LSP-shaped declaration queries (`hover_by_name`, `type_of_name`, raw `search_declarations`) — folded into
  `lean_lookup(kind = "declaration")` and `lean_lookup(kind = "proof_search")`;
- raw module-query access (`lean_query`) — driven internally by `lean_context(kind = "proof_position")`,
  `lean_trial(kind = "command")`, and `lean_status(kind = "file_diagnostics")`;
- text search and placement policy (`source_search`, `project_scan`, `mathlib_placement`);
- split reference aliases (`references_in_file`, `references_in_project`) — unified under
  `lean_lookup(kind = "references")`.

When one of these is needed to implement a deeper tool, keep it private and compose it inside the host or worker. A need
to expose it directly is treated as evidence that a better proof-work abstraction is missing.

## Budgets And Caching

Worker APIs enforce per-field and total output budgets before IPC. The host adds hard caps for candidate lists,
reference fanout, and response shape. The worker owns module snapshot reuse and reports cache status, output bytes, and
phase timings for declaration-context queries. The host keeps a bounded content-hash LRU in front of every module query,
single-file and batched alike, so a repeat query on an unmodified file never enters a project's mailbox. The content
hash *is* the invalidation: an edited file produces a different key, so the cache carries no TTL. Only module queries
are cached — a name lookup, proof attempt, or verification would key on a name whose meaning changes whenever any
`.olean` in the closure is rebuilt, with no hash in the key to notice.

Frame size is controlled by query shape, not transport tuning: no public tool requests a whole info tree, bulk rendered
types, or unbounded source/project scans.

## Lake compilation options

Source checks resolve the target file's options through the pinned Lake API before checking or looking up a cached
result. The helper evaluates configuration afresh without resolving or materializing dependencies or entering Lake's
build graph. Root and direct dependency files use their own package and library settings, including saved root `-K`
options and current `require ... with` options. Source checks for transitive dependencies fail explicitly rather than
guess their configuration; lookups of already-built declarations are unaffected.

The helper uses the same guardian-owned native reader as external checks. Both root and dependency configuration caches
live under the existing generation's private `inputs/` directory; framework loading creates no project `.lake` directory
and changes no shared cache or trace. Real package paths, cwd and `__dir__` remain intact. Parent shared custody
protects metadata resolution until the guardian receives the same complete read union. These are cooperating read-only
programs; arbitrary user configuration IO is not sandboxed. Private inputs survive uncertain cleanup and are removed
only after actual scope closure. Files outside a configured library and file-less trials use Lake's root-package
defaults.

Configuration admission shares the existing broker lease mutex and activity counter. A held lease refuses before
configuration work; a drain waits without dispatch. An admitted utility keeps its activity guard through the existing
owned cleanup job, including after its async waiter disappears. One preparation group may coexist with a full resident
actor quota, so option discovery does not evict a warm worker or request exclusive project access.

The worker receives typed options, including `weak.*` options. It resolves their types after imports load.
`-Dname=value` compiler arguments use Lean's parser; other extra compiler arguments return an explicit unsupported
error. Source, query and compilation options all contribute to the result-cache key. `freshness.elaboration_options`
records the project options and the actual host heartbeat limit, which overrides the project's `maxHeartbeats`.

A helper starts once per source request, including cache hits. Evaluated configuration is not cached: it may read files
or environment variables beyond the lakefile. Discovery has one 30-second admission/execution budget; cancellation
requests owned group cleanup. Configuration errors return no proof verdict and never retry with defaults.

## Queue and execution deadlines

`runtime.request_timeout_millis` now also limits time waiting for dispatch, including mailbox admission and waiting
behind earlier requests. The existing worker operation timeouts remain separate; they do not bound the whole request
across setup, multiple operations, and retries. It starts when the project creates the message, not while opening the
project.

An expired queued request returns `queue_wait_timeout` and cannot run later. The actor and caller share one atomic
start-or-expire decision. Expired messages stay in the bounded mailbox until the actor removes them; they do not reach
Lean. Once dispatch starts, the existing worker operation limits apply. Cancellation of a running request waits for the
worker's cleanup reply. Neither cancellation nor expiry counts as a successful proof check.

Before source-backed actor calls, the broker keeps metadata/options preparation and project admission within one private
boundary. Only typed `project_admission_timeout` or `request_cancelled` errors returned from that boundary record a
named `execution.target_check` with stage `project_admission` on retained requests. The actor operation was not
dispatched; preparation or earlier internal checks may still have effects or uncertain cleanup. Aggregate lifetime
classification continues to use its existing operation evidence. A post-admission error cannot acquire this receipt, and
no stored historical result is reclassified. Native-reader ACK precedes its read-admission/payload boundary, so a lost
post-ACK result still cannot infer payload non-dispatch from a null start field or missing domains.

## Native ownership and artifact custody

Each project actor owns its actual worker child and its reaper. Ordinary and semantic generations share one durable
actor quota in the canonical private ownership registry. A native guardian owns the generation's inherited process group
and retains the complete set of shared artifact descriptors. It acknowledges durable custody before a worker loads a
library, runs an initializer, imports a module, or executes a submitted body. Cooperating writers acquire the same
canonical domains exclusively and check ownership fences before dispatch.

The parent first asks the worker's runtime-free readiness entry for an exact prepared manifest snapshot. A warm probe
only reads under shared custody. A cold preparation uses the existing owned drain before acquiring an opening lock or
native slot, then performs source publication, Lake build and manifest publication through the one writer helper.
Prepared domains cannot expand within a generation. A changed snapshot or artifact union refuses before the body and
retires the actor; a later request prepares a fresh actor. The refused request is never replayed.

Parent-only cache and index reads hold close-on-exec shared descriptors through parsing and freshness observation.
External native tools use `native-reader`, whose controlled bootstrap transfers the full read union to the same guardian
before spawning its one payload. An external read receipt does not assert the loaded shim ABI. All read/write domain
unions use the same canonical lock-path order.

A timeout or cancelled caller returns no proof result. A confirmed request-bound child exit remains available through
replacement failure and late reaping. Capacity is released only when the actor is sealed and its actual process scopes
close. If wait authority is lost but group absence is confirmed, resource capacity and retained observation can recover
without inventing an exit status or changing the unknown proof outcome. Retained request identities cannot silently
redispatch after expiry, restart, lost replies, or full retention; safe storage exhaustion returns backpressure.

Server death is observed by the guardian through the private channel and exact parent identity. If the guardian dies
while the parent survives, the parent retains its actual unreaped child and uses the same owned-group termination
primitive as other supervised commands. Failed kill, or loss of both parent and guardian, retains passive ownership
fences until actual group absence; recovery never signals a PID recovered from disk. The supported boundary requires
descendants to remain in the inherited process group. Deliberate detachment and weaker Linux process observation do not
become successful proof results. The actor quota bounds admitted generations, not every paused pre-admission OS process;
one bounded preparation utility may coexist without evicting a warm actor. Independent uncoordinated writers and
arbitrary user IO outside declared cooperating domains are outside this lock protocol.

## Pure compatibility and external input

`lean_compatibility` with explicit `mode: "pure_v1"` describes compiled expectations, project/toolchain identity and the
canonical helper/ownership namespace. It starts no worker, preparer or native helper and claims no loaded runtime. Its
expected binding fingerprints come from the same signature declarations and canonical encoder as typed host bindings.
Every cold or replacement session validates those expectations plus the pinned header/runtime before exposing a body.
Default `loaded` mode remains an explicit manual handshake returning actual worker receipts. A client requiring pure
discovery must retain the mode on failure; older servers reject it before their loaded handler.

External `native-reader` payloads have closed stdin by default. Explicit `--stdin` stages one EOF-terminated stream of
at most 16 MiB in the generation's own inputs. The caller supplies an owned pipe read end or regular file and must not
share read aliases during capture; terminals and sockets are refused. After guardian admission, the bootstrap hashes and
rewinds the exact read-only descriptor installed at payload fd0. Optional `stdin_sha256` and `stdin_bytes` receipts bind
that offered descriptor. They do not attest consumption or loaded ABI. Callers require a matching receipt and a
complete, non-intervened result before using output; missing output never authorizes implicit replay.

An optional `--cancel-fd` accepts a private, already nonblocking pipe read end. The helper seals inherited descriptors
against exec; any byte or EOF requests the existing owned-group shutdown while the caller stays alive. Cancellation is
intent, not a successful result or permission to replay. Optional `payload_usage` comes from the sole wait that reaped
the actual payload: wall time, user/system CPU and peak RSS normalized to bytes. Kernel accounting may include
descendants the payload waited for; it does not measure the entire native group. Missing wait evidence produces no usage
facts, and RSS remains observation only.
