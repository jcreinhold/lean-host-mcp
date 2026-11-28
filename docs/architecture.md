# Architecture

This contributor reference explains how one server owns a Lake checkout and the processes it starts, how it admits work,
and when a result stays valid. Start with the [README](../README.md) for setup and the [tool catalog](tool-catalog.md)
for public contracts.

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
- [One server per checkout](#one-server-per-checkout) and [the rules and their tests](#rules-and-their-tests)

## Crate Layout

```
crates/lean-host-mcp/src/
  main.rs                clap CLI, shared broker setup, rmcp stdio entry
  transport_http.rs      private axum/rmcp Streamable HTTP entry
  server.rs              LeanHostService tool registration
  broker.rs              ProjectBroker: the served root, worker slots, short-run permits, idle retirement, the reaper
  broker/control.rs      worker activity snapshots for lean_runtime, and the activity count
  broker/host_artifacts.rs the installed shims a worker reads before it opens
  broker/module_setup.rs lake setup-file and lake query as short runs; the import closure and module sources
  broker/semantic_file.rs file-backed semantic proof search through a short run
  broker/verify_file.rs  file checks through one-shot Lean programs
  project.rs             LeanProject actor: one serialized controller and worker
  project/runtime_config.rs the actor's memory budget, Lean heap ceiling, deadline, mailbox and restart limits
  project/restart.rs     why a worker cycled: cause names, which causes count toward the restart limit, the tally
  project/semantic_runtime.rs the semantic-search runtime that install builds and a server opens, and its cache
  worker_retirement.rs   keeps an actor's capacity until each tracked child has exited
  project_lock.rs        fair read and write turns per Lake root, and the read shares of short runs
  build.rs, build/       lean_build: the [build] and [recipes] config, Lake word checks, each step's group, the capped log
  lean_run.rs            short runs: one lean or lake program in its own process group, with capped output
  children.rs            children.lock and the per-child files; startup's wait for an earlier server's processes
  host_preparation.rs    the worker's host-artifact modes: readiness, and the preparation an installer runs
  runtime_compatibility.rs the one native compatibility contract for workers
  tools/                 semantic.rs facade; declaration/proof_search/proof_action/position internals; build.rs
  projections.rs         stable MCP projections from lean-rs worker types
  lake_meta.rs           minimal Lake-project metadata
  lake_options.rs, lake/ Lake compile options and setup through Lake's own API, without building imports
  toolchain.rs           the project's lean-toolchain pin, its elan toolchain and its worker
  cache.rs               bounded module-query cache, valid while the worker that computed an entry lives
  envelope.rs            internal operation envelope adapted to { data, errors, trust } at the public boundary
  requests/              named requests: the SQLite request table, one launch per id, recovery at start
  request_log.rs         content-free summaries of semantic calls
  store.rs               the server store: server.lock, then children.lock, build logs, the semantic runtime cache
  config_file.rs         the tracked and local config files at the Lake root
  config_schema.rs       every config setting, in one table
  cli/                   install, install-worker, config init and doctor processes
  status.rs              build identity and resolved settings for the status reply
  cancellation.rs        request cancellation shared by async callers and the worker owner
  error.rs, trust.rs     the one error type of tool handlers; the trust facts in each reply
  digest.rs              SHA-256 as lowercase hex, for every receipt, stamp and fingerprint
  ilean.rs               a reader for Lean's .ilean reference indices
  semantic_search.rs     the lean-semantic-search runtime adapter
  smoke.rs, diagnosis.rs the install smoke verdict in worker.json; resolution-health messages

crates/lean-host-mcp-worker/src/
  lib.rs                 the deployment record in worker.json, which the server writes and both sides check; no Lean
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

The broker owns one required host-binding subset per supported compiled-header ABI. Its binding fingerprints come from
the same signature declarations and encoder as the typed host bindings, and every new or replacement session checks the
subset before it serves. After each completed call the actor checks the host facts the child reported and keeps a
receipt that binds them to its session and worker generation. The receipt is kept only while that generation is current.
`lean_status` with `include: ["worker"]` reads the receipts of a project's resident workers without opening or asking a
worker; check replies do not carry them.

`lean_runtime` reads the pool under the broker registry mutex and changes nothing. Preparation utilities, startup locks,
queued jobs, and background recycling/prewarming all join the activity count it reports. The guard survives caller
disconnects and registry removal through cleanup, so a retired controller cannot start new maintenance. Worker permits
stay held until confirmed shutdown; neither a snapshot nor a shutdown request proves resources are free. See the
[public contract](tool-catalog.md#worker-activity).

## Project And Worker Boundary

`ProjectBroker` serves one canonical Lake root and refuses a call that names another. It invalidates a changed manifest,
coalesces opens, and admits up to `max_workers_per_project` actors within the `max_workers` slots. Each `LeanProject`
actor has one dedicated controller thread, one host handle, its own cache and restart state, and one in-flight job. The
parent never links `libleanshared`; the per-toolchain worker child does.

Typed `ProjectMessage` variants carry job metadata and a typed reply channel into the bounded actor mailbox.
`ProjectActorState::handle_message` runs the selected operation through shared lifecycle handling. Queued admission is
FIFO, cancellable, and deadline-bounded; a separate FIFO queue admits cold projects. A worker session borrows the
controller's host handle and stays within synchronous dispatch. It cannot cross `.await` or be shared between tasks.

The broker reserves a slot before opening a worker. Opening, resident, and retiring actors count against the cap until
confirmed shutdown. An actor-owned process tracker covers each worker and its replacement generations. Once the actor
seals, the retirement registry retains its slot until every direct child is reaped. Late exit observation restores
capacity, including after cleanup-thread creation failure; uncertain cleanup retains the slot. This registry lives in
one server process. After a crash the next server waits for the dead server's workers through `children.lock` (see
[one server per checkout](#one-server-per-checkout)). The cap counts actor runtimes, not memory bytes or each transient
companion process. Short runs have their own bound, `broker.max_lean_runs`.

The installed worker reads the project's own build in place. At launch the broker names the project's import roots in
Lean's search order, from `lake_meta::manifest_artifact_roots`: the root package's `.lake/build/lib/lean`, then each
manifest package's. It admits an import only when the request's roots are exactly the launch roots, in order, then the
installed shim roots, and while no root holds a package whose compiled code the worker runs, such as `Init` or
`Init.olean`; any module names are allowed. A worker whose launch roots are not Lake build outputs starts anyway and
refuses each import with the reason. No lock guards these roots: only the server that launched the worker builds the
checkout, and it stops the worker before each build. A child that exits before it connects makes the server wait out the
30-second startup deadline it gives the SDK. The server refuses to start with `LEAN_PATH` set, since the worker would
refuse it before connecting. The host copies no files, hashes no build outputs, and writes no import plan.

Before it takes an opening turn or a worker slot, the broker asks the worker's readiness mode for the descriptor of the
shims `install` built (`broker/host_artifacts.rs`), and the worker loads the shims that descriptor names. The mode reads
and builds nothing. If the shims are missing or stale, the open fails with the install command; the server never builds
shims.

Startup stays serialized after caller disconnects and rechecks the per-project limit. Idle retirement skips a borrowed
actor and one with queued work. Metadata-only degraded responses and pure `.ilean` reads use
`project_identity_without_worker`; valid module-query cache hits bypass the actor mailbox. A full mailbox returns
structured retryable pressure.

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

A pooled environment is a snapshot from its import, so no build may change the artifacts under a live worker. A build
stops the project's workers and waits until each has exited before Lake runs (see
[turns and builds](#turns-and-builds)). As a backstop, each job carries the finished-build count of its read turn, and
the actor keeps the count under which the live generation first loaded imports. A higher count cycles the worker before
the job as `artifacts_rebuilt`, which is not execution taint, and drops every pooled profile.

Worker death never authorizes replay: Lean elaboration and tactics may perform IO. The opt-in
`retry_tainted_non_positive` policy allows one retry of a non-positive trial/verification verdict after a mid-call
recycle. Earlier failure evidence stays separate from the final attempt. A completed retry retains a visible warning; a
tainted or interrupted final attempt remains unusable and uncacheable. Runtime facts and timing interpretation are
listed in [operations](operations.md#internal-runtime-facts).

The worker loads the bundled `lean_rs_host_*` shim capabilities from `lean-rs-host`. The source-backed proof-search lane
uses `lean-semantic-search-runtime`, which owns its Lean payload, provenance, materialization, and build. The host
selects cache roots and toolchain/sysroot: `install` builds the runtime into the store, and a server only opens it.
Consumers do not declare, link, or import either support package.

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
version, host version, worker protocol, digest support at build time, and runtime smoke outcome. Protocol determines
wire compatibility, so the server serves a worker that another host version built. `install` and `install-worker --auto`
rebuild such a worker: the worker and host crates release together, and a release can keep the protocol. A legacy
sidecar without protocol information leaves the worker handshake authoritative. The installer builds the shims with
plain Lake in the generation's own directory, and the digests of the shim files bind that build.

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
it and answers only while that generation lives: a build stops every worker before Lake runs, so no build has changed
what a live generation read. A probe made while another generation lives, or none, drops the entry. Execution taint
prevents insertion, and unknown query variants bypass caching.

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

Callers hold the project's read turn, so no build writes an index while it is read. Reference scans return `NotBuilt`
when the build directory is absent, or `Present` after a scan. Declaration queries distinguish an absent project build
tree, an absent/unreadable module index, and a parsed index. Callers translate these outcomes into public statuses,
warnings, and artifact trust facts.

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

The helper is a short run (`lean_run.rs`), like a file check: one program in its own process group, with capped output.
Both root and dependency configuration caches live in the run's private directory under the store's `runs/`; framework
loading creates no project `.lake` directory and changes no shared cache or trace. Real package paths, cwd and `__dir__`
remain intact. The run holds the request's read turn, so no build the server runs changes the files it reads. These are
cooperating read-only programs; arbitrary user configuration IO is not sandboxed. Private inputs stay until the run's
group has exited. Files outside a configured library and file-less trials use Lake's root-package defaults.

Every short run takes a permit from the broker's `broker.max_lean_runs` semaphore before it starts and keeps it until
its group has exited, even past its reply. A run that finds no permit by its deadline does not start. Short runs also
join the broker's activity count, but take no worker slot, so option discovery does not evict a warm worker or request
exclusive project access.

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
was not dispatched; preparation or earlier internal checks may still have effects or uncertain cleanup. A short run
whose group outlives its stop reports an unknown outcome, never a run that did not start.

A named request (`requests/`) runs through the same tools. `RequestRunner::run` checks the id and arguments, plans the
root, kind and deadline, then inserts the row in a `BEGIN IMMEDIATE` transaction that returns the stored row when the id
exists, so of two parallel calls with one id only one inserts and starts. A check takes its read turn, then commits
`mark_running`, then calls the tool. A build commits it in its `OnRunning` hook, which runs once the build holds the
write turn and its workers have exited, before its first step. Nothing starts before its row says `running`, and a row
in a final state never changes: a trigger refuses the update. A check that fails at its deadline waits until the
supervisor sees its worker request stop before its row says `timed_out`. A dropped connection ends only a client's wait;
a call without an id keeps the old rule and is cancelled when its connection drops.

## Turns and builds

`project_lock.rs` gives each canonical Lake root one fair `tokio::sync::RwLock`. A request takes at most one turn and
keeps it in a task-local slot, so a nested check in the same request reuses its read turn instead of queueing behind a
waiting build. A check takes its read turn before it pins an actor or starts a short run, and holds it until the request
ends. A short run whose group outlives its request keeps a share of that turn (`project_lock::read_share`) until the
group exits. A build takes the write turn; a request that holds a read turn cannot take it. Tokio grants turns in
arrival order, so a queued build holds back later checks, and each waiter keeps its place until its deadline. Each
finished write turn adds one to the root's finished-build count.

`lean_build` (`build.rs`, `tools/build.rs`) plans the run from the `[build]` and `[recipes]` config, takes the write
turn, stops the project's workers and waits until each has exited (`ProjectBroker::stop_workers`), then runs the guard
and each step as `lake <args>` in the Lake root. Each step runs in its own process group with its own child file (see
[one server per checkout](#one-server-per-checkout)), and all output goes to one capped log in the store. Cancellation
or the build's deadline stops the running step's whole group. The next check starts a fresh worker; a build that changes
nothing still stops the workers. The turn ends only after each step's group has exited, so no check starts while a
process of the build still runs. Lake decides what to build and in which order; the server checks only the words it
hands to Lake. Do not add an import graph walk or a build scheduler: a test scans the build path for Lake's import-graph
queries. `lean_imports` reads each file's transitive closure from `lake setup-file --no-build`, and the sources of the
modules the root built from one `lake query +MODULE:lean`, under a read turn.

Lean writes each `.olean` to a temporary file and renames it into place (`src/library/module.cpp` in Lean), so a reader
never sees a torn file and a file a worker has mapped never changes under it. The turns prevent the one hazard left: a
check that loads some modules from before a build and some from after.

After a lakefile edit, `lake setup-file` rewrites Lake's configuration cache, and checks hold read turns at once, so two
can start that rewrite together. Lake locks the cache itself (`importConfigFile` in `Lake/Load/Lean/Elab.lean`): readers
share the trace lock and a rewrite holds it alone, so the cache stays whole. Of two processes that start a rewrite at
the same moment, Lake fails one with "could not acquire an exclusive configuration lock"; that check reports Lake's
error, and a retry reads the new cache.

The turns order only the Lake and Lean runs this server starts. A `lake build` run outside the server can rewrite
`.olean` files while a check reads them, and neither the turns nor the finished-build count see it. That is outside the
contract.

### Rules and their tests

The server keeps no TLA+ model. A model checks a description of the design, not this code, so each rule names the test
that checks the code. "None yet" marks a gap.

| Rule | Test |
| --- | --- |
| One server per checkout: a second server on the same store exits at startup and names the first. | `store.rs`: `second_lock_names_the_holder`, `a_refused_start_names_the_holder_and_leaves_children_alone`, `empty_record_still_refuses`; `tests/http_transport.rs`: `second_server_on_one_store_names_the_first` |
| Within one server, turns go in arrival order. | `project_lock/tests.rs`: `builds_and_checks_take_turns_in_arrival_order` |
| Checks share the turn and a build runs alone: checks on one project hold read turns at once, a build waits for every read turn and share, and no check loads imports while a build runs. | `project_lock/tests.rs`: `two_requests_hold_read_turns_at_once`, `a_queued_writer_goes_before_a_later_reader_which_sees_its_finish`, `a_read_share_keeps_a_build_waiting_after_its_request_ends`; `tests/project_workers.rs`: `a_check_sent_during_a_build_waits_and_answers_from_the_new_build`; `tests/requests.rs`: `two_file_checks_run_lean_at_once` |
| A build stops the project's workers, and each has exited before Lake starts; the next check starts a fresh worker on the new build. | `tests/project_workers.rs`: `a_build_stops_the_project_workers_before_lake_starts` |
| A worker serves trials, proof positions, lookups and checks of modules with no source file from the project's own build, path dependencies included. | `tests/project_workers.rs`: `a_trial_imports_a_dependency_module_and_prints_its_axioms`, `a_proof_position_and_a_lookup_answer_from_the_project_build`, `a_module_without_a_source_file_is_checked_in_a_worker` |
| A worker admits an import only through its launch roots, in order, then the shim roots, while no root holds `Init`. | `tests/project_workers.rs`: `an_import_through_other_roots_is_refused`, `an_import_root_that_holds_init_is_refused` |
| Workers stay within `max_workers` and `max_workers_per_project` while checks, trials and builds share a project. | `tests/many_clients.rs`: `many_clients_share_one_project` |
| At most `broker.max_lean_runs` short runs at once; a run that gets no permit by its deadline never starts. | `lake_options.rs`: `a_run_that_gets_no_permit_by_its_deadline_never_starts`; `tests/requests.rs`: `with_one_lean_run_two_file_checks_take_turns`, and `two_file_checks_run_lean_at_once` for the default of two |
| A short run stops its whole group on cancel, at its time limit, past its output cap, or when its leader leaves members running, and its reply never waits on a process outside the group. | `lean_run.rs`: `a_cancelled_run_writes_no_inputs_and_never_starts`, `a_run_past_its_limit_stops_the_group`, `output_past_the_cap_stops_the_group`, `a_leftover_member_holding_stdout_open_is_stopped_without_waiting_for_eof` |
| No build starts between a check's `setup-file` and its use of the imports. | `project_lock/tests.rs`: `a_nested_read_reuses_the_turn_while_a_writer_waits` |
| The next server serves only after the last one's children exit: it stops each recorded group whose file lock is held, then waits for `children.lock`. | `children.rs`: `a_held_child_file_gets_its_group_stopped_and_the_lock_frees`, `a_free_children_lock_clears_stale_files_and_passes_to_children`; `tests/requests.rs`: `the_next_server_serves_only_after_a_killed_servers_build_exits`, `a_killed_servers_workers_exit_and_the_next_build_runs` |
| A server never signals a group whose file lock is free. | `children.rs`: `a_child_file_whose_lock_is_free_is_never_signalled`, `stop_group_kills_members_a_leader_left_only_while_one_holds_the_lock` |
| No worker slot returns while its worker may live, and a server killed while a worker starts takes no slot from the next server. | `tests/project_workers.rs`: `after_a_kill_during_worker_start_the_next_server_has_every_slot`; SDK `lean-rs-worker-parent/tests/worker_shutdown.rs` tests reaping and slot counts in one server |
| A wait for a turn ends at its deadline. | `project_lock/tests.rs`: `a_read_wait_ends_at_its_deadline_or_on_cancel`, `a_write_wait_ends_at_its_deadline_or_on_cancel_and_leaves_the_line` |
| One launch per request ID; a record before launch; the same ID with other arguments refused; a stored result never changes; a restart ends every recorded request; a call without an ID runs with a server ID and no record. | `requests/store_tests.rs`: `parallel_inserts_of_one_id_insert_once`, `one_id_holds_one_call`, `a_final_row_never_changes`; `tests/requests.rs`: `a_named_request_runs_once_and_keeps_its_result`, `the_same_build_sent_three_times_at_once_starts_one_lake`, `another_call_on_an_id_fails_and_makes_no_row`, `a_build_outlives_its_killed_client`, `a_killed_server_interrupts_its_build_and_runs_the_queued_one`, `an_ended_request_survives_a_server_kill`, `a_direct_verify_names_a_server_id`; `tests/many_clients.rs`: `many_clients_share_one_project`; `tests/http_transport.rs`: `install_and_serve_keep_no_state_in_an_empty_home` |
| A file check reports what `lake build` reports: Lean checks the whole file, header included, and drops messages it marks silent. | `tests/e2e.rs`: `lean_trial_command_and_lean_status_file_diagnostics`, for `file_diagnostics`; `worker_file_checks_see_the_header_and_drop_silent_messages`, for `lean_verify` and `file_outline`, which use the SDK file driver (`LeanRsHostShims/InfoTree.lean`). File-backed proof search keeps it through lean-semantic-search's goal elaboration (`selectGoalSnapshotWithSetup` in `LeanCompat/Frontend.lean`), which that repository's `LeanSemanticSearchTest.lean` checks. |
| A file check with an out-of-date import builds nothing and replies `needs_build`, naming each module Lake lists. | `broker/module_setup.rs`: `no_build_exit_names_the_targets_lake_lists`; `diagnosis.rs`: `out_of_date_setup_is_needs_build_with_one_stale_row_per_target`; `tests/build_requests.rs`: `a_file_check_with_a_stale_import_names_the_module_to_build` |

## One server per checkout

One server serves a Lake checkout and starts every Lake and Lean run on it, so it can order checks and builds in memory.
Two `flock` locks in the store hold this across processes; the kernel frees each one only when the last process that
shares its open file description exits, however it exits. Nothing in the server unlocks either one: an unlock would free
the lock for every process that shares it.

`server.lock` admits one server per store. The server takes it exclusively, close-on-exec, and writes its pid and Lake
root into the file; a second server exits at startup and names the first. The lock binds the store, not the checkout:
two servers that use different stores for one Lake root are outside the contract.

`children.lock` outlives the server. Once it holds `server.lock`, the server takes `children.lock` exclusively and
clears its close-on-exec flag, so every child it starts, and every descendant of those, shares the lock. Before a build
step or a short run, the server creates a file `starting-…` in the store's `children/`, locks it, and starts the child
with that descriptor too (`process_group::spawn_inheriting`). Once the child runs, the server renames the file to the
child's process group; it deletes the file when it has confirmed the group's exit. A held file lock therefore means a
process of that group still runs, and a free one means none does. Workers and the `lake` run that opens the semantic
runtime have no file, since the SDK and the search crate start them. They hold `children.lock` like every child, and a
worker exits with status 125 once the pipe to its server closes.

At startup, after `server.lock`, `store::hold` takes `children.lock` (`children.rs`):

- If `children.lock` is free, no process of an earlier server runs. The server deletes every file in `children/` and
  serves.
- If it is held, the server sends SIGTERM to each recorded group whose file lock is held, waits up to 5 seconds, then
  sends SIGKILL to each group still held. It deletes each file whose lock is free, then waits up to 30 seconds for
  `children.lock`, and serves once it has the lock. It never signals a group whose file lock is free and never looks for
  processes by name.
- If the lock is still held after that wait, the server exits with an error that names the groups still running and
  counts the children that had not yet been named. The error says that processes the server does not record, such as a
  worker whose pipe to the dead server has not closed yet, may hold the lock too, and that `lsof <store>/children.lock`
  lists every holder.

The wait covers every process that kept the inherited descriptor. A process that closed it can outlive the startup wait,
and one that left its group escapes the signals. If every member left a group while one still holds the file's lock, the
group number can name another group by the time the signal goes. Recovery steps for a refused start are in
[operations](operations.md#a-refused-start).

`install` and `install-worker --lake-root` hold the store as a server does, so each refuses while a server serves the
root. `install` builds the semantic runtime into `<store>/semantic-runtimes`; a serving server only opens it, and a
runtime that needs a build fails the call with the install command.
