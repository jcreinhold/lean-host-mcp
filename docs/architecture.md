# Architecture

This contributor reference explains worker ownership, admission, artifact custody, and result validity. Start with the
[README](../README.md) for setup and the [tool catalog](tool-catalog.md) for public contracts.

The five semantic tools expose a declaration-oriented proof workflow. The host resolves names and proof-position
selectors into bounded jobs, sends them to a supervised Lean worker, and projects typed results into the public
response. Tools do not apply source edits automatically. Executed Lean code can perform IO with the server user's
privileges; the worker is not an OS sandbox.

- [Crate layout](#crate-layout) and [transport](#transport-boundary)
- [Runtime control](#runtime-control) and [worker ownership](#project-and-worker-boundary)
- [Proof API](#declaration-centric-proof-api), [tool routing](#tool-semantics), and
  [cache validity](#budgets-and-caching)
- [Reference indices](#reference-indices)
- [Lake options](#lake-compilation-options), [deadlines](#queue-and-execution-deadlines) and
  [turns and builds](#turns-and-builds)
- [Artifact custody](#native-ownership-and-artifact-custody) and
  [external inputs](#pure-compatibility-and-external-input)

## Crate Layout

```
crates/lean-host-mcp/src/
  main.rs                clap CLI, shared broker setup, rmcp stdio entry
  transport_http.rs      private axum/rmcp Streamable HTTP entry
  server.rs              LeanHostService tool registration
  broker.rs              ProjectBroker: LRU pool, idle eviction, per-call routing, the reaper
  broker/control.rs      process-local worker activity snapshots and expiring admission leases
  broker/module_setup.rs lake setup-file as a native reader; the import closure for lean_imports
  broker/verify_file.rs  file checks through one-shot Lean programs
  project.rs             LeanProject actor: one serialized controller and worker
  worker_retirement.rs   keeps an actor's capacity until each tracked child has exited
  project_lock.rs        fair read and write turns per Lake root, the shared kernel guard, proxy turns
  build.rs, build/       lean_build: the [build] and [recipes] config, Lake word checks, the run in one helper
  shared_writer.rs       the SDK writer helper, including its build log mode
  native_reader.rs       one-shot Lean programs in the SDK's native reader scope
  host_preparation.rs    the worker's host artifacts, prepared before any guarded generation
  native_socket.rs       where the SDK's short-lived sockets live
  artifact_reads.rs      parent-only artifact reads that hold shared descriptors until parsing ends
  runtime_compatibility.rs the one native compatibility contract for writers and workers
  tools/                 semantic.rs facade; declaration/proof_search/proof_action/position internals; build.rs
  projections.rs         stable MCP projections from lean-rs worker types
  lake_meta.rs           minimal Lake-project metadata
  lake_options.rs, lake/ Lake compile options and setup through Lake's own API, without building imports
  toolchain.rs           the project's lean-toolchain pin, its elan toolchain and its worker
  cache.rs               bounded module-query cache, valid while the worker that computed an entry lives
  envelope.rs            internal operation envelope adapted to { data, errors, trust } at the public boundary
  requests/              named requests: the SQLite request table, one launch per id, recovery at start
  request_log.rs         content-free summaries of semantic calls
  store.rs               the server store: one-server lock, build logs
  config_file.rs         the tracked and local config files at the Lake root
  config_schema.rs       every config setting, in one table
  cli/                   install, install-worker, config init and doctor processes
  status.rs              build identity and resolved settings for the status reply
  cancellation.rs        request cancellation shared by async callers and the worker owner
  error.rs, trust.rs     the one error type of tool handlers; the trust facts in each reply
  ilean.rs               a reader for Lean's .ilean reference indices
  semantic_search.rs     the lean-semantic-search runtime adapter
  smoke.rs, diagnosis.rs a smoke test for an installed worker; resolution-health messages

crates/lean-host-mcp-worker/src/
  main.rs, authority.rs  worker child; serves one Lake project from its own build through the roots named at launch
```

`server.rs` is glue only. Tool implementations resolve a project through `ProjectBroker`, read source files when needed,
and send one typed semantic job to the project controller.

## Transport Boundary

Stdio and Streamable HTTP construct the same `LeanHostService` over one `ProjectBroker`. The private binary module
`transport_http.rs` owns axum/rmcp wiring; HTTP types do not enter the tools, broker, or project runtime. Independent
HTTP sessions share the broker's worker caps, queues, and lifecycle state. Stdio ends on client EOF; `--bind` selects
HTTP.

The SDK handles stateless discovery and per-request capability/version metadata for MCP 2026-07-28, with
initialize/session support for legacy clients. Both paths use the same service and request table. HTTP rejects every
present `Origin` with 403 and validates loopback Host headers. See [client setup](../README.md#connecting-an-mcp-client)
for transport selection and [operations](operations.md) for runtime failure handling.

Runtime pressure remains in the semantic `errors` channel. Transport/protocol failures use HTTP or MCP errors. The
semantic facade projects internal `Response<T>` envelopes into `{ data, errors, trust }`; operational tools have their
own contracts in the [tool catalog](tool-catalog.md). Output carrier selection belongs at the service boundary.

## Runtime control

`lean_compatibility(mode = "loaded")` uses the same broker admission, actor mailbox, startup accounting, and worker-slot
lifecycle as semantic calls. Its actor job opens a checked host session but submits no target proof. The broker owns one
required host-binding subset per supported compiled-header ABI; loaded actor/generation receipts remain separate from
that expected contract. Worker-session configuration enforces the subset after every recycle, and semantic trust
preserves actual receipts independently of telemetry verbosity. This replaces caller-side binary/shim profiles rather
than adding a second admission queue.

`lean_runtime` coordinates admission under the broker registry mutex. `acquire` requires an idle pool; `drain` pauses
new admission while admitted work finishes. Preparation utilities, startup locks, queued jobs, and background
recycling/prewarming all participate in the activity count. The guard survives caller disconnects and registry removal
through cleanup, so an evicted controller cannot start new maintenance.

A lease expires on monotonic time and is process-local. Its owner renews or releases it with the returned token; status
only observes it. `retire_idle` closes idle mailboxes once admitted activity finishes, while worker permits remain held
until confirmed shutdown. Idle expiry pauses during a lease, but independent artifact-writer demand may retire affected
caches. Neither a snapshot nor a shutdown request proves resources are free. The lease does not preempt work, reserve
system RAM, or coordinate unrelated processes. See the
[public contract](tool-catalog.md#worker-activity-and-admission-leases).

## Project And Worker Boundary

`ProjectBroker` owns a bounded LRU pool keyed by canonical Lake root. It resolves projects, invalidates changed
manifests, coalesces opens, and admits up to `max_workers_per_project` independent actors under the global `max_workers`
cap. Each `LeanProject` actor has one dedicated controller thread, one host handle, its own cache and restart state, and
one in-flight job. The parent never links `libleanshared`; the per-toolchain worker child does.

Typed `ProjectMessage` variants carry job metadata and a typed reply channel into the bounded actor mailbox.
`ProjectActorState::handle_message` runs the selected operation through shared lifecycle handling. Queued admission is
FIFO, cancellable, and deadline-bounded; a separate FIFO queue admits cold projects. A worker session borrows the
controller's host handle and stays within synchronous dispatch. It cannot cross `.await` or be shared between tasks.

The broker reserves a slot before opening a worker. Opening, resident, and retiring actors count against the cap until
confirmed shutdown. An actor-owned process tracker covers ordinary workers, semantic companions, and replacement
generations. Once the actor seals, the retirement registry retains its slot until every direct child is reaped. Late
exit observation restores capacity, including after cleanup-thread creation failure; uncertain cleanup retains the slot.
This registry is process-local and does not reconcile server crashes or Lake descendants. The cap counts actor runtimes,
not memory bytes or each transient companion process. External Lean processes are outside it.

The installed worker reads the project's own build in place. At launch the broker names the project's import roots in
Lean's search order, from `lake_meta::manifest_artifact_roots`: the root package's `.lake/build/lib/lean`, then each
manifest package's. For its whole life the worker holds shared the kernel lock of every artifact domain under those
roots and of the installed shims. It admits an import only when the request's roots are exactly the launch roots, in
order, then the installed shim roots, and while no root holds a package whose compiled code the worker runs, such as
`Init` or `Init.olean`; any module names are allowed. A worker that cannot take custody of its project at launch, for
example after the manifest dropped a package, starts anyway and refuses each import with the reason. A child that exits
before it connects makes the server wait out the 30-second startup deadline it gives the SDK, and leaves an owner record
in phase `dispatching` that the SDK never closes: every later build that uses that owners directory waits until someone
removes the record. The server refuses to start with `LEAN_PATH` set for this reason, since the worker would refuse it
before connecting. The host copies no files, hashes no build outputs, and writes no import plan.

Same-project startup stays serialized after caller disconnects and rechecks the per-project limit. The broker can evict
an idle LRU actor, but cannot evict a borrowed actor or one with queued work. Metadata-only degraded responses and pure
`.ilean` reads use `project_identity_without_worker`; valid module-query cache hits bypass the actor mailbox. A full
mailbox returns structured retryable pressure.

### Import memory and session reuse

`max_import_residue_bytes` is the policy that cleanly recycles a child. `runtime.lean_max_memory_kib` is a separate Lean
heap backstop: crossing it during allocation can throw a C++ exception through the Rust FFI frame and abort the child
with exit status 134. It is derived as `budget + max(budget, 8 GiB)`, converted to KiB. A fixed 8 GiB ceiling previously
sat below the 9 GiB residue floor and aborted file three of a kan-proofs sweep before a clean recycle could run. The 8
GiB minimum headroom covers the measured 4.4–6.3 GiB gap between residue and Lean heap; doubling handles larger budgets.
When overriding the heap ceiling, preserve headroom above the residue budget.

RSS is observation only. It includes clean shared `.olean` mappings and can fall while private retained memory grows.
The child reuses matching imports, so retained state no longer grows per call. Imports still accumulate: `importModules`
with `loadExts := true` retains its environment regions, and only the first import maps compacted regions. Later imports
materialize their whole closure as private copies, including overlap.

Measurements on real per-file kan-proofs headers explain the byte budget:

- The first import used 10–18 MB; later imports used 2.0–4.5 GB each.
- Four imports cost 9.60 GiB on one profile sample and 16.00 GiB on another.
- Accumulated `LeanWorkerImportStats::non_memory_mapped_region_bytes` predicted physical footprint growth with slope
  1.09 and R² 0.96. Import count, compacted bytes, and RSS did not provide the same signal.
- Retaining a warm environment cost about 70 MiB, compared with 2.0–4.5 GB to import again.

The session pool therefore retains outgoing environments, with capacity independent of the residue budget (default eight
profiles). Returning to a held profile avoids an import. `max_imports` remains a 32-import backstop for unusable
zero-valued statistics. Planned `import_residue` cycles do not consume the actor's abnormal-restart budget; the
supervisor has a separate all-causes restart backstop.

Above 60% of the residue budget, a quiet actor can cycle as `import_residue_idle` after a two-second grace. It prewarms
the latest profile only if recently served (within one minute), and cycles at most once per quiet period. The
`import_residue_idle : import_residue` ratio measures how much restart cost lands between requests.

A pooled environment is a snapshot from its import, so no writer may change the artifacts under a live worker. The
worker holds the shared kernel lock for its whole life, and a build stops the project's workers before it takes the
exclusive lock (see [turns and builds](#turns-and-builds)). As a backstop for writers in other processes, each job
carries the finished-build count of its read turn, and the actor keeps the count under which the live generation first
loaded imports. A higher count cycles the worker before the job as `artifacts_rebuilt`, which is not execution taint,
and drops every pooled profile.

Worker death never authorizes replay: Lean elaboration and tactics may perform IO. The opt-in
`retry_tainted_non_positive` policy allows one retry of a non-positive trial/verification verdict after a mid-call
recycle. Earlier failure evidence stays separate from the final attempt. A completed retry retains a visible warning; a
tainted or interrupted final attempt remains unusable and uncacheable. Runtime facts and timing interpretation are
listed in [operations](operations.md#internal-runtime-facts).

The worker loads the bundled `lean_rs_host_*` shim capabilities from `lean-rs-host`. The source-backed proof-search lane
uses `lean-semantic-search-runtime`, which owns its Lean payload, provenance, materialization, and build. The host
selects cache roots and toolchain/sysroot. Consumers do not declare, link, or import either support package.

### Toolchain-Readiness Gate

`WorkerBinary::resolve_ready` checks the supported table, elan/worker layout, recorded header digest, wire protocol, and
runtime smoke result before spawn. It looks in the project's worker directory, which `toolchain::workers_dir` picks for
both the server and `install`: `LEAN_HOST_MCP_WORKERS_DIR`, then `[install] workers_dir` from the project's config
files, then `.local/lean-host-mcp/workers` under the Lake root. Project discovery first checks the lakefile and
manifest, so readiness tests need a real pinned Lake root.

| Verdict | Meaning |
| --- | --- |
| `Unsupported` | Numbered pin absent from the supported table; returned before filesystem probes, with the nearest supported version (ties prefer newer) |
| `ToolchainNotInstalled` / `NotInstalled` | Missing elan toolchain or worker binary |
| `Stale` | Current `lean.h` differs from the recorded build digest |
| `Incompatible` | Recorded worker wire protocol differs from this parent |
| `Unusable` | Recorded post-install runtime smoke failed despite a matching header |
| `UnknownPin` | Installed non-numbered pin, allowed with an advisory |
| `Ready` | May spawn, with an advisory if provenance or runtime smoke evidence is missing |

Header drift takes precedence over protocol mismatch, which precedes failed smoke. Hard verdicts refuse before spawn
with one line that names the worker directory and the fix, `SERVER install --lake-root PROJECT`. Protocol mismatch maps
to `IncompatibleWorker` with that command as its recovery command; the other hard verdicts map to `BadProject`.
Advisories survive runtime failure and are projected into public warning issues. Installation checks the pure
supported-window verdict before building. Resident actor selection uses metadata and health, while source-option
resolution also rechecks worker readiness, including the current header digest.

### Worker Provenance Sidecar

`install-worker` writes `worker.json` beside the installed binary: toolchain id, full `lean.h` SHA-256, build-time Lean
version, host version, worker protocol, digest support at build time, and runtime smoke outcome. Host version is
informational; protocol determines wire compatibility. A legacy sidecar without protocol information leaves the worker
handshake authoritative.

The smoke test opens a worker session importing `Init` and inspects `Nat.add_zero`. Failure exits installation nonzero
but retains the binary and failed record so `--list` and readiness can explain the problem. Missing provenance or smoke
evidence produces an advisory rather than an automatic refusal. `install-worker --list` reports independent support,
header/build, runtime, and protocol columns; a digest match alone does not establish runtime ABI compatibility.

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

`tools::semantic` owns the five semantic tool families and the public trust projection. The
[tool catalog](tool-catalog.md) is authoritative for modes and request/result schemas. Internal operation modules retain
narrow names such as `proof_state`, `inspect_declaration`, and `try_proof_step`; these are not MCP registrations.

The facade routes through shared plumbing:

- `position.rs`: proof context, command trials, file diagnostics, and references.
- `declaration.rs` and `declaration_inventory.rs`: individual inspection and file/module outlines.
- `proof_search.rs`: source-backed semantic retrieval with bounded lean-rs declaration-search fallback.
- `proof_action.rs`: proof-step trials and declaration verification.

Project-scope references read `.ilean` indexes without a worker. Source-backed modes elaborate current snapshots; module
inventories can supplement or fall back to built indexes. These paths attach typed artifact freshness facts to `trust`.
Quiet presentation removes telemetry, while retaining proof-relevant trust and warnings. Transport progress
notifications, including token-gated heartbeats, stay outside worker operations.

## Scope Boundary

The public proof workflow uses declaration names and proof-position selectors. Low-level elaboration, kernel checks,
term/meta operations, raw cursor queries, and raw module-query access remain private components of that workflow. Text
grep and Mathlib placement policy are outside this server. File and project references share one public mode.
Operational status, compatibility, and lifecycle tools have separate contracts; they are not declaration queries.

## Budgets And Caching

Worker APIs cap rendered fields and total output before IPC. The host adds candidate and reference-fanout caps. Public
queries do not request whole info trees or unbounded project scans, so query shape controls frame size.

`ModuleQueryCache` stores bounded file-keyed outcomes for single and batched queries. A key includes the file path,
source bytes, query/selector shape and output budgets, session/freshness imports, elaboration options, and the
finished-build count of the request's read turn. There is no TTL. Each entry names the worker generation that computed
it and answers only while that generation lives: the worker holds the shared kernel lock for its whole life, so no
writer has changed what it read. A probe made while another generation lives, or none, drops the entry. The count alone
is not enough, since a short build in another process can end between two reaper ticks and never move it. Execution
taint prevents insertion, and unknown query variants bypass caching.

A valid hit bypasses the actor mailbox, but source option discovery still runs. Name-only inspection/search, proof
attempts, and verification are not cached: their meaning depends on an imported environment and execution policy that
cannot be identified by source content alone. The worker separately owns bounded module snapshot reuse.

## Reference indices

The private parent module [`ilean.rs`](../crates/lean-host-mcp/src/ilean.rs) reads built reference and declaration
indices without a Lean worker. Project reference lookup and declaration inventory consume its two query functions.

### Reader boundary

`lake build` writes one `.ilean` per module under `<project>/.lake/build/lib/lean/`. Each index records definition and
usage sites within that module's source. Project-scope `lean_lookup(kind="references")` reads these indices instead of
re-elaborating every source file; the former worker scan took about 3 s/file, or 27 minutes for 500 modules.

`references_to` and `declarations_in_module` use `serde_json` and `walkdir`, adding no Lean linkage or worker
capability. Raw JSON types and path mapping stay private.

Both functions acquire shared artifact-read custody before opening an index. Reference scans return `Unavailable`
without reading when custody cannot be established, `NotBuilt` when the build directory is absent, or `Present` after a
scan. Declaration queries distinguish unavailable custody, an absent project build tree, an absent/unreadable module
index, and a parsed index. Callers translate these outcomes into public statuses, warnings, and artifact trust facts.

`ReferenceIndex` carries hits, parsed/skipped module counts, and `stale_sources`. An unreadable, malformed, unsupported,
or uncovered individual index is skipped and counted. Staleness checks compare source/index mtimes only for modules that
contributed hits; missing metadata is treated as not stale. This is a limited build-freshness observation, not an exact
source-content binding. `ReferenceLocation` carries the source file, definition/reference kind, and 0-based LSP
coordinates.

A `files` restriction narrows reads only when every source path maps unambiguously to an existing index. Paths outside
the project, dotted components, non-`.lean` extensions, and missing computed indices trigger a full walk. The caller
still filters the returned hits to the requested files, so uncertain filename mapping cannot silently omit a match. The
recursive walk covers the project's build directory, excluding dependency indices under `.lake/packages/`.

### Scan performance evidence

Measurements from `benches/ilean_reference_scan.rs` over a 1431-module project (76 MB of `.ilean`), using `Fin` as the
hot name with 9095 hits before the result cap:

| Query | Before | After |
| --- | --- | --- |
| Whole project, hot name | 427 ms | ~120 ms |
| Whole project, no hits | 478 ms | ~136 ms |
| One requested file | 271 ms | ~0.5 ms |

A phase split on the same corpus measured `stat` at 0.010 s, reads at 0.066 s, and parsing at 1.32 s: about 95% of the
original cost was parsing. The reader therefore parses once, projects `references` and `decls` separately, filters
reference-map entries during deserialization, and narrows index reads for file-restricted queries. Allocation scales
with matching entries rather than all ~400k entries in that corpus. It retains no cache between calls.

The scan remains serial and runs under `spawn_blocking` to keep file I/O and parsing off tokio workers. Serial order
also keeps the user-visible stale-source warning order stable. Revisit parallelism if a real whole-project scan exceeds
about one second. These figures are historical measurements, not checks rerun during this documentation cleanup.

### Supported v5 schema

The original on-disk verification used Lean v4.31.0-rc1's `Lean/Data/Lsp/Internal.lean` (`RefIdent`, `RefInfo`,
`ModuleRefs`) and `Lean/Server/References.lean` (`Ilean`, `Ilean.load`). The reader accepts only version `5`.

Each module is one JSON object:

```jsonc
{ "version": 5,
  "module": "Demo.Foo.Bar",
  "directImports": [ ["Std.Data.List", false, true, false] ],   // ignored
  "references": {
    // Keys are compressed-JSON RefIdent objects, not flat arrays.
    "{\"c\":{\"m\":\"Demo.A\",\"n\":\"Demo.A.foo\"}}": {          // const: defining module, name
      "definition": [3, 4, 3, 7],                                // range; null is allowed
      "usages": [ [5, 2, 5, 5, "Demo.B.bar"], [6, 8, 6, 11] ]     // optional fifth parent-declaration name
    },
    "{\"f\":{\"m\":\"Demo.B\",\"i\":\"x\"}}": {}               // local variable: ignored
  },
  "decls": { "Demo.A.foo": [3,4,3,7, 3,4,3,7] }                  // declaration range ++ selection range
}
```

For a requested name `N`, reference parsing keeps constant keys with `n == N`, emits a non-null `definition` as `def`
and each usage as `ref`, and resolves all locations against that index's module source (`Demo.A` becomes
`<root>/Demo/A.lean`). Definition and usage ranges accept four coordinates plus an optional fifth parent-declaration
string. Declaration ranges require exactly eight coordinates.

In the measured corpus, `references` occupied about 97% of bytes and `decls` about 3%. Separate projections skip the
unneeded subtree, so a valid JSON document with an unexpected `decls` shape does not break reference lookup, and the
reverse holds for declaration lookup.

The version gate runs after parsing and discards the entire document for any version other than `5`. Lean's compressed
JSON sorts object keys, placing `version` last: in one sample it began at byte 116,287 of 116,299. A preliminary version
probe would therefore scan nearly every byte twice. Only a parse failure triggers a second, version-only probe, which
can still classify an unfamiliar future shape as `UnsupportedVersion` rather than malformed JSON.

Tests in `ilean.rs` cover version-last objects, future versions with unreadable shapes, independent projections,
narrowing/fallback behavior, exact array arities, and stale-source reporting.

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

`runtime.request_timeout_millis` supplies a deadline from broker project acquisition through opening, mailbox admission,
and waiting behind earlier requests. The same absolute deadline follows the job into the actor. Source-option resolution
before project acquisition has its own budget. Worker operation timeouts remain separate; they do not bound the whole
request across preparation, multiple operations, and retries.

An expired queued request returns `queue_wait_timeout` and cannot run later. The actor and caller share one atomic
start-or-expire decision. Expired messages stay in the bounded mailbox until the actor removes them; they do not reach
Lean. Once dispatch starts, the existing worker operation limits apply. Cancellation of a running request waits for the
worker's cleanup reply. Dropping the caller future also signals cancellation; actor-owned cleanup retains capacity until
completion. Neither cancellation nor expiry counts as a successful proof check.

Before source-backed actor calls, the broker keeps metadata/options preparation and project admission within one private
boundary. A typed `project_admission_timeout` or `request_cancelled` error from that boundary means the actor operation
was not dispatched; preparation or earlier internal checks may still have effects or uncertain cleanup. Native-reader
ACK precedes its read-admission/payload boundary, so a lost post-ACK result still cannot infer payload non-dispatch from
a null start field or missing domains.

A named request (`requests/`) runs through the same tools. `RequestRunner::run` checks the id and arguments, plans the
root, kind and deadline, then inserts the row in a `BEGIN IMMEDIATE` transaction that returns the stored row when the id
exists, so of two parallel calls with one id only one inserts and starts. A check takes its read turn, then commits
`mark_running`, then calls the tool. A build commits it in its `OnRunning` hook, which runs once the build holds the
write turn and the writer lock and before its first step. Nothing starts before its row says `running`, and a row in a
final state never changes: a trigger refuses the update. A check that fails at its deadline waits until the supervisor
sees its worker request stop before its row says `timed_out`. A dropped connection ends only a client's wait; a call
without an id keeps the old rule and is cancelled when its connection drops.

## Turns and builds

`project_lock.rs` gives each canonical Lake root one fair `tokio::sync::RwLock`. A request takes at most one turn and
keeps it in a task-local slot, so a nested check in the same request reuses its read turn instead of queueing behind a
waiting writer. A check takes its read turn before it pins an actor or starts a native reader, then the shared kernel
guard on the project's artifacts, and holds both until the request ends. A build takes the write turn, then the
exclusive kernel lock through the SDK writer helper; a request that holds a read turn cannot take the write turn. Tokio
grants turns in arrival order, so a queued build holds back later checks, and each waiter keeps its place until its
deadline. Each finished write turn adds one to the root's finished-build count.

Workers hold the shared kernel lock for their whole generation. When a writer in another process announces demand, the
reaper queues a proxy write turn on that root, so new checks wait behind it, and retires the idle, unborrowed actors
whose domains the writer wants. A queued proxy turn always ends as a finished build, even when the demand ended before
the grant.

`lean_build` (`build.rs`, `tools/build.rs`) plans the run from the `[build]` and `[recipes]` config, takes the write
turn, stops the project's workers and waits until each has exited (`ProjectBroker::stop_workers`), opens one writer
helper, and runs the guard and each step in it, with all output in one capped log in the store. The next check starts a
fresh worker. A build that changes nothing still stops the workers. The turn ends only after the helper and every
process of the build have exited. Lake decides what to build and in which order; the server checks only the words it
hands to Lake. Do not add an import graph walk or a build scheduler: a test scans the build path for Lake's import-graph
queries. `lean_imports` reads each file's transitive closure from `lake setup-file --no-build` under a read turn.

After a lakefile edit, `lake setup-file` rewrites Lake's configuration cache, and checks hold read turns at once, so two
can start that rewrite together. Lake locks the cache itself (`importConfigFile` in `Lake/Load/Lean/Elab.lean`):
readers share the trace lock and a rewrite holds it alone, so the cache stays whole. Of two processes that start a
rewrite at the same moment, Lake fails one with "could not acquire an exclusive configuration lock"; that check reports
Lake's error, and a retry reads the new cache.

A raw `lake build` takes no kernel lock, so neither the turns nor the finished-build count see it. Running one while the
server reads the project is outside the contract.

### Rules and their tests

The server keeps no TLA+ model. A model checks a description of the design, not this code, so each rule names the test
that checks the code. "None yet" marks a gap.

| Rule | Test |
| --- | --- |
| Within one server, turns go in arrival order. | `project_lock/tests.rs`: `builds_and_checks_take_turns_in_arrival_order` |
| No check loads imports while a build runs. | `project_lock/tests.rs`: `a_queued_writer_goes_before_a_later_reader_which_sees_its_finish`; `tests/project_workers.rs`: `a_check_sent_during_a_build_waits_and_answers_from_the_new_build`; across processes, none yet |
| A build stops the project's workers, and each has exited before Lake starts; the next check starts a fresh worker on the new build. | `tests/project_workers.rs`: `a_build_stops_the_project_workers_before_lake_starts` |
| A worker serves trials, proof positions, lookups and checks of modules with no source file from the project's own build, path dependencies included. | `tests/project_workers.rs`: `a_trial_imports_a_dependency_module_and_prints_its_axioms`, `a_proof_position_and_a_lookup_answer_from_the_project_build`, `a_module_without_a_source_file_is_checked_in_a_worker` |
| A worker admits an import only through its launch roots, in order, then the shim roots, while no root holds `Init`; a worker that cannot take custody of its project refuses at once. | `tests/project_workers.rs`: `an_import_through_other_roots_is_refused`, `an_import_root_that_holds_init_is_refused`, `a_worker_whose_launch_roots_leave_the_project_refuses_at_once` |
| Workers stay within `max_workers` and `max_workers_per_project` while checks, trials and builds share a project. | `tests/many_clients.rs`: `many_clients_share_one_project` |
| No build starts between a check's `setup-file` and its use of the imports. | `project_lock/tests.rs`: `a_nested_read_reuses_the_turn_while_a_writer_waits` |
| One writer at a time, across processes and server restarts. | SDK `lean-toolchain/src/writer_lock/tests.rs`; `tests/requests.rs`: `a_killed_server_interrupts_its_build_and_runs_the_queued_one`; `tests/many_clients.rs`: `many_clients_share_one_project` |
| No lock release while a process of the build may live. | SDK `writer_lock/tests.rs`: `inherited_same_description_loan_retains_exclusion_until_owned_child_exit` |
| No worker slot returns while its worker may live. | SDK `lean-rs-worker-parent/src/crash_ownership/tests/guarded_release.rs`: `guarded_child_that_exits_before_hello_closes_its_record_and_slot`, for a worker that exits before it connects; otherwise none yet, and SDK `lean-rs-worker-parent/tests/worker_shutdown.rs` tests reaping and slot counts nearby |
| A killed server's workers and guardians exit with it, and the next server's build runs without waiting for them. | `tests/requests.rs`: `a_killed_servers_workers_exit_and_the_next_build_runs` |
| A wait for a turn ends at its deadline. | `project_lock/tests.rs`: `a_read_wait_ends_at_its_deadline_or_on_cancel`, `a_write_wait_ends_at_its_deadline_or_on_cancel_and_leaves_the_line` |
| One launch per request ID; a record before launch; the same ID with other arguments refused; a stored result never changes; a restart ends every recorded request; a call without an ID runs with a server ID and no record; checks on one project hold their read turns at once. | `requests/store_tests.rs`: `parallel_inserts_of_one_id_insert_once`, `one_id_holds_one_call`, `a_final_row_never_changes`; `tests/requests.rs`: `a_named_request_runs_once_and_keeps_its_result`, `the_same_build_sent_three_times_at_once_starts_one_lake`, `another_call_on_an_id_fails_and_makes_no_row`, `a_build_outlives_its_killed_client`, `a_killed_server_interrupts_its_build_and_runs_the_queued_one`, `an_ended_request_survives_a_server_kill`, `a_direct_verify_names_a_server_id`, `checks_on_one_project_run_at_once`; `tests/many_clients.rs`: `many_clients_share_one_project`; `tests/http_transport.rs`: `install_and_serve_keep_no_state_in_an_empty_home` |
| A file check reports what `lake build` reports: Lean checks the whole file, header included, and drops messages it marks silent. | `tests/e2e.rs`: `lean_trial_command_and_lean_status_file_diagnostics`, for `file_diagnostics`. `lean_verify`, `file_outline` and file-backed proof search break this rule: they use the SDK file driver (`LeanRsHostShims/InfoTree.lean`), which checks the body without its header and keeps silent messages. The fix needs an SDK release. |
| A file check with an out-of-date import builds nothing and replies `needs_build`, naming each module Lake lists. | `broker/module_setup.rs`: `no_build_exit_names_the_targets_lake_lists`; `diagnosis.rs`: `out_of_date_setup_is_needs_build_with_one_stale_row_per_target`; `tests/build_requests.rs`: `a_file_check_with_a_stale_import_names_the_module_to_build` |
| `serve` refuses a socket directory that is unsafe or too long for a worker socket. | `tests/stdio_lifecycle.rs`: `serve_refuses_a_socket_directory_too_long_for_worker_sockets`; `native_socket.rs`: `worker_socket_path_accepts_maximum_and_refuses_one_byte_more` |

## Native ownership and artifact custody

Each project actor owns its actual worker child and its reaper. Ordinary and semantic generations share one durable
actor quota in the canonical private ownership registry. A native guardian owns the generation's inherited process group
and retains the complete set of shared artifact descriptors. It acknowledges durable custody before a worker loads a
library, runs an initializer, imports a module, or executes a submitted body. Cooperating writers acquire the same
canonical domains exclusively and check ownership fences before dispatch.

Cooperating artifact writers announce process-bound demand on the same canonical physical domains before waiting for
exclusive artifact custody. The broker checks that handoff gate before borrowing a resident actor or admitting idle
maintenance. Contention pauses affected new calls within their existing cancellation/deadline budget; admitted calls
finish normally. A separate 100 ms observation in the existing reaper retires only affected idle, unborrowed actors,
even when ordinary idle expiry is disabled or a lease preserves warm caches. It does not advance or release that lease.
Retirement still uses the ordinary process owner, so an acknowledgement never releases capacity or artifact custody
before actual scope closure. Nonoverlapping actors remain available. Matching uses the actor's retained actual
read-domain receipts, including frozen dependencies and companions, rather than a newly reduced manifest. A brief
competing reader gate can also cause conservative backpressure or idle retirement; it is not evidence of a writer
identity. The protocol does not claim FIFO ordering among multiple writers or defeat OS scheduling/filesystem stalls.

The generic `reader` and `read-proxy` admissions still refuse writer demand before Ready. They can be composed across
multiple physical domains, so waiting while retaining an earlier shared lock would create a reader/writer cycle. A
metadata caller holding no other artifact read or write custody may explicitly select `read-proxy --single-domain-wait`.
This one-domain proxy retries only lock admission under its original absolute admission deadline, closes transient gates
before waiting, and observes its private FIFO caller on each iteration and again before Ready. EOF or deadline expiry
emits no Ready. Metadata executes once in the caller after that receipt; neither a metadata body nor a target check is
retried. SDK multi-domain readiness probes retain the fail-fast default.

Whole-domain admission owns the complete fixed input set. Its opted-in pure helpers report normal exit 75 only for typed
pre-custody contention (`--report-contention`); every malformed receipt, descriptor, helper failure or uncertain cleanup
remains a hard error. The SDK verifies successful pre-transfer classification, releases all tentative shared locks
(including confirmed EOF-proxy helper closure), then waits within the original deadline and cancellation budget. It
never waits for a later domain while retaining an earlier partial set, and never repeats a payload or metadata body. A
full-set gate preflight avoids spawning repeated helpers while known writer demand remains; it grants no read custody,
and the actual nonblocking transfer checks close the following race. Native payload bootstrap and cold worker admission
finish the full set before guardian acknowledgement. A warm worker's guardian retains earlier generation locks, so warm
contention uses the existing confirmed pre-body generation cycle instead of waiting behind its own custody. Graph,
epoch, inheritance and guardian failures after admission are not retried.

The supervisor retains a failed replacement process when cleanup cannot confirm its exit. An earlier generation's exit
cannot satisfy structured shutdown for that replacement. Consequently an internal idle recycle failure also keeps the
project's capacity occupied until shutdown confirms cleanup, or conservatively retains it if cleanup remains uncertain.

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
without inventing an exit status or changing the unknown proof outcome. A named request never runs twice: a retry or a
lost reply returns its stored row, and a restart ends a row that said `running` without starting it again. A store
failure refuses new rows with a retryable error.

Server death is observed by the guardian through the private channel and exact parent identity. If the guardian dies
while the parent survives, the parent retains its actual unreaped child and uses the same owned-group termination
primitive as other supervised commands. Failed kill, or loss of both parent and guardian, retains passive ownership
fences until actual group absence; recovery never signals a PID recovered from disk. The supported boundary requires
descendants to remain in the inherited process group. On Linux a short-lived payload waiter adopts orphaned descendants.
After reaping the payload it checks for children across all groups and sessions. A survivor at verdict time prevents
success. The bootstrap retains the guardian and its acknowledged read loans until the exact waiter exits and its bounded
report is collected. A lost waiter or report supplies no verdict. The outer owner separately checks closure of the
original scope. That closure does not establish that an escaped process stopped writing, including after a waiter crash.
This check cannot detect a descendant that detached and exited before the verdict. The actor quota bounds admitted
generations, not every paused pre-admission OS process; one bounded preparation utility may coexist without evicting a
warm actor. Independent uncoordinated writers and arbitrary user IO outside declared cooperating domains are outside
this lock protocol.

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
