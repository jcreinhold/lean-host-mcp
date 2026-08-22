# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0/).

<!-- ASSISTANT: add new entries under [Unreleased]; `release-lean-host-mcp` promotes them on tag. -->

## [Unreleased]

## [0.11.0] - 2026-08-22

### Changed

- **Widened the toolchain window head to Lean 4.34.0-rc2 and added 4.33.1 to the window.** The `lean-rs` /
  `lean-semantic-search` 0.7 line is unchanged (already at lean-rs 0.7.3 / lean-semantic-search 0.7.2): lean-rs 0.7.3's
  `SUPPORTED_TOOLCHAINS` adds 4.33.1 (new `lean.h` digest backporting the 4.34.0-rc1 TSan instrumentation —
  byte-identical ABI probe) and 4.34.0-rc2 (new digest implementing the sticky-rc fix — no struct declaration changes,
  byte-identical probe against rc1), so the supported window is `4.30.0 ..= 4.34.0-rc2`. The fixture pin, the worker
  the server is built and tested against, and the head-toolchain literals in the test suite all move to v4.34.0-rc2.
  The Rust floor is unchanged at 1.94.
- **Upgraded rmcp from 1.8 to 3.1.4.** Tracks the MCP 2025-11-25 spec alignment: `Content` is renamed to
  `ContentBlock`, version negotiation now runs through rmcp's `supported_protocol_versions()` (the explicit
  `ProtocolVersion::LATEST` pin in `get_info` was dropped as redundant), and `CallToolResult` gained a `resultType`
  field that rmcp omits for legacy sessions. Newer clients may now negotiate protocol 2025-11-25, under which the
  Streamable HTTP transport enforces the `MCP-Protocol-Version` header. The Rust floor moved from 1.91 to 1.94 (which
  also matches the lean-rs 0.7.3 floor).

## [0.10.0] - 2026-08-11

### Changed

- **Widened the toolchain window head to Lean 4.34.0-rc1.** The `lean-rs` / `lean-semantic-search` 0.7 line is
  unchanged; upstream lean-rs 0.7.2 added 4.34.0-rc1 to `SUPPORTED_TOOLCHAINS` (new `lean.h` digest confined to TSan
  instrumentation — byte-identical ABI probe, all 88 required symbols resolve), so the supported window is
  `4.30.0 ..= 4.34.0-rc1`. The fixture pin, the worker the server is built and tested against, and the head-toolchain
  literals in the test suite all move to v4.34.0-rc1. The Rust floor is unchanged at 1.91.

## [0.9.1] - 2026-08-10

### Fixed

- **The e2e suite raced itself under the default parallel test runner.** The two rebuild tests move the fixture's
  `Handles.olean` mtime forward to stand in for `lake build`, while the two residue-budget tests assert across
  multi-second sleeps that the worker serving that same import never recycles — one test's bump landed inside
  another's stability window and both failed with `artifacts_rebuilt` recycles that neither caused. The four tests
  now share a `serial_test` key (`handles_olean`) that confines the exclusion to exactly them; the rest of the
  suite still runs in parallel.

## [0.9.0] - 2026-08-04

### Changed

- **Adopted the lean-rs / lean-semantic-search 0.7 line and widened the toolchain window head to Lean 4.33.0-rc2.**
  `lean-rs-worker-{child,parent,protocol}`, `lean-toolchain`, and the four `lean-semantic-search-*` crates move from 0.6
  to 0.7; upstream's `SUPPORTED_TOOLCHAINS` now groups 4.33.0-rc2 with rc1 (byte-identical ABI probe), so the supported
  window is `4.30.0 ..= 4.33.0-rc2`. The fixture pin, the worker the server is built and tested against, and the
  head-toolchain literals in the test suite all move to v4.33.0-rc2. The Rust floor is unchanged at 1.91.

## [0.8.0] - 2026-07-28

### Fixed

- **The Lean heap ceiling aborted workers before the residue budget could recycle them.** `runtime.lean_max_memory_kib`
  was a fixed 8 GiB, below the 9 GiB floor under the residue budget, so at Mathlib scale the ceiling always fired first
  — and it does not fire gracefully. Its documented behaviour, "overrun surfaces as an ordinary Lean error in the `ok`
  payload", is false in this embedding: Lean's allocator signals the overrun by throwing a C++ exception, which cannot
  unwind through the Rust frame at the shim FFI boundary, so the child aborts with exit status 134 and
  `fatal runtime error: Rust cannot catch foreign exceptions, aborting`. Observed on a three-file kan-proofs sweep:
  files one and two answered, file three aborted the child mid-import at 1.7 GiB of residue against a budget that could
  never be reached. The ceiling's default is now derived — twice the residue budget, because the gap has to cover what
  the heap counts and the budget does not, which that abort put at 4.4-6.3 GiB — so the clean recycle is always strictly
  ahead of the abort, and raising the budget carries the ceiling with it. Setting `runtime.lean_max_memory_kib` at or
  below the budget explicitly is now warned about at startup. The same sweep now completes with zero restarts.
- **The residue budget could exceed the machine it was sizing for.** Its 9 GiB floor was unconditional, so on any host
  under about 12 GiB the budget was larger than total RAM and the child grew until the OS killed it — the exact failure
  the bound exists to prevent. The derived default is now also capped at what is left after one in-flight import, which
  makes the existing below-floor startup warning reachable on the machines it was written for.
- **A child abort carried no record of why.** `LeanWorkerRestartReason::ChildAbort` kept only the operation name, and
  the child's stderr is drained once at reap and retained nowhere else, so a supervisor-absorbed abort — the path behind
  the read-only verify/proof-state degraded verdict — reported `child_abort operation=…` and nothing more. It now
  carries the exit status and the child's message, which is what identified the heap-ceiling abort above.
- **A pooled Lean environment could kill the worker mid-request.** Upstream, `Environment.extensions` is sized once at
  import and can never grow afterwards, so an environment held across a later import that registers an environment
  extension — which is what every `initialize` block and every `register_simp_attr` does — is permanently short.
  Elaborating any `namespace`, `section`, or `open … in` against it panics inside Lean once per out-of-range slot, and
  the child dies mid-query; the tool call comes back as a degraded batch. Session pooling made this reachable. lean-rs
  now stamps each imported environment with an extension-registration epoch and drops any held environment whose stamp
  no longer matches, in the worker child and in `SessionPool` alike. Requires the patched lean-rs; no wire protocol
  change, no config change, and an import that registers nothing is unaffected. A related fix keeps a staleness eviction
  from permanently disabling the worker's reuse hint, which would otherwise have made every later session open count
  against `max_imports` even when it was a pool hit.

- **A worker restart the server could not classify was recorded as planned.** `worker_internal` — the bucket for a
  restart whose cause this build does not recognise, including any cycling policy a future lean-rs adds — did not count
  toward `runtime.worker_restart_limit`, so a child cycling repeatedly for an unknown reason never tripped the
  crash-loop breaker. `diagnosis` already treated the same cause as disrupting a verdict, so the two disagreed. It now
  counts.
- **A child abort reported as a restart reason was misclassified.** `LeanWorkerRestartReason::ChildAbort` had no arm in
  the cause mapping and fell through to `worker_internal`, which (see above) was then treated as planned and
  non-abnormal. It now maps to `child_abort`, matching how the same event is classified when it arrives as a worker
  error instead of a restart reason.

### Changed

- **The supported Lean window floor is now 4.30.0; workers for 4.27.0–4.29.1 are unservable.** lean-rs 0.6.1 dropped
  those toolchains: their `libleanshared` does not export the `collectAxioms` symbol the mandatory host shim
  references, so the window claimed support the runtime never had. A project pinned to a dropped toolchain is now
  rejected at open with the out-of-window verdict, and `install-worker --prune` removes workers already installed for
  one. Pulled in through the existing `"0.6"` lean-rs requirements; no API change.
- **`scripts/memory_stability.py` reports import residue, the quantity the recycle policy actually reads.** It reported
  RSS only, which is an observation and not a policy input, so the harness could not show whether a long-lived server
  was approaching a recycle. New `peak_import_residue_mib` / `final_import_residue_mib` / `import_residue_budget_mib`
  summary fields, and a kan-proofs workload (`scripts/memory_stability.kanproofs.json`) alongside the fixture one.
  Measured over 60 calls on a Mathlib-scale project: 60/60 `ok`, generation unmoved, 0 restarts, residue flat at 1786
  MiB against a 9216 MiB budget.
- **The worker recycle bound is denominated in bytes, not in imports.** `WORKER_MAX_IMPORTS = 4` was the server's only
  reclamation mechanism, and it counted the wrong thing: an import's unreclaimable residue is its *whole closure*
  (`.olean` compacted regions are position-dependent, so only the first import in a process is memory-mapped and every
  later one re-copies even already-mapped modules privately). Measured against real per-file headers at Mathlib scale,
  the first import costs 10–18 MB and each later one 2.0–4.5 GB — so the same count of four imports costs 9.6 GiB on one
  workload and 16.0 GiB on another. The server now accumulates the bytes the child reports and recycles on
  `runtime.worker_import_residue_budget_mib`, defaulting to a quarter of system RAM split across `broker.max_projects`
  and clamped to 9–12 GiB. A `max_imports = 32` backstop remains underneath, reachable only when a child reports
  zero-valued import statistics. New restart cause `import_residue`; planned, so it does not count toward
  `runtime.worker_restart_limit` and does not taint a verdict.
- **The session pool is sized independently of the recycle bound.** New `runtime.worker_session_pool_capacity` (default
  8) sets how many imported environments one child keeps warm. It was previously tied to the recycle bound, which meant
  the child's LRU eviction never ran and a `changed_coverage` sweep over more than four files re-imported on every step.
  A held environment costs about 70 MiB against 2.0–4.5 GB for the import it saves.
- **Residue cycles are taken while the project is idle where possible.** Above 60% of the budget the project actor waits
  for its next call with a short timeout instead of blocking indefinitely; on a quiet tick it recycles the worker and
  re-imports the most recently served profile, so both the spawn and the import land between calls rather than inside
  one. Reported as the `import_residue_idle` restart cause. The `import_residue_idle : import_residue` ratio is the
  operational signal: idle dominating means the cost is off the critical path, reactive dominating means the budget is
  too low for how continuously the workload runs. Contexts without a tokio runtime keep the plain blocking receive.
- **`runtime.import_residue_bytes` and `runtime.import_residue_limit_bytes`** now ride the telemetry block, so the
  quantity the policy acts on is observable rather than inferred.
- **One import per session instead of one per call.** The root cause of the server's memory growth was upstream: the
  worker child rebuilt its host session on every `OpenHostSession`, so every tool call ran a full `importModules`, and
  under `loadExts := true` those imported regions are never reclaimable. Child RSS therefore grew with the *number of
  calls*, not with the workload — measured at ~1.18 GiB per import, 2.9 GiB across eleven repeat opens. lean-rs now
  reuses a session whose `(project_root, mode, imports, import_profile)` match. Warm `search_for_proof` on the fixture
  fell from 1.41 s to 1.26 s (−10.3%, p = 0.01) on top of the batching win that preceded it.
- **One Lean heap budget replaces four resident-memory thresholds.** `runtime.lean_max_memory_kib` (default 8 GiB,
  `LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB`) is enforced inside the child by `lean_internal_set_max_memory`, so an elaboration
  that exhausts it fails as ordinary Lean-domain data inside the `ok` payload and the worker keeps serving. Removed:
  `runtime.worker_rss_post_job_restart_kib`, `runtime.worker_rss_hard_kill_kib`, `runtime.worker_rss_sample_millis`,
  `runtime.import_switch_rss_soft_kib`, the `invalid RSS config` startup check, and the forced recycle every 64
  requests. All four measured resident memory, which counts the shared, clean, mmapped `.olean` pages every
  Mathlib-scale worker maps at startup — so they fired on healthy workers while doing nothing about the growth above.
  Dropping the hard-kill watchdog also removes the supervisor's polling read loop, which forked `/bin/ps` on macOS every
  250 ms during an in-flight call. RSS is still sampled once per call for `runtime.rss_kib`; no policy reads it.
  `[runtime]` goes from 10 knobs to 5.
- `scripts/rss_threshold_sweep.py` is now `scripts/memory_stability.py`: it replays a workload `--repeats` times against
  **one** server and reports whether the worker generation advances, rather than sweeping thresholds that no longer
  exist.
- **Worker cycling is bounded, and switching import profiles no longer costs a re-import.** Session reuse removes growth
  with call *count*, but not with import *count*: an environment imported with `loadExts := true` is never reclaimed, so
  a child that keeps importing keeps growing — unbounded, that ended in an OS `SIGKILL` before call 180. Dropping the
  outgoing environment before importing the next one does not help, because under `loadExts := true` an import's
  compacted regions survive the environment that owns them. The worker child therefore parks the outgoing session in a
  bounded pool and the parent cycles the child on a budget (see the byte-denominated bound above, which replaced this
  entry's original import *count*). An alternating workload became the same workload as a repeating one: 200 calls over
  two profiles perform 2 imports and never recycle the worker, where before they cycled roughly every other call. On
  `benches/worker_roundtrip.rs` an `inspect_declaration` that switches profiles every iteration runs in 2.17 ms against
  2.36 ms for one that does not — the switch is now a key comparison and a round trip, where it used to be ~787 ms of
  respawn and re-import. The supervisor's own all-causes restart backstop is raised to clear the planned cycle rate,
  because its default of 16 per minute counted these planned cycles and, once exhausted, refused to spawn any
  replacement at all.
- **`artifacts_rebuilt` now catches a rebuild under any profile the child may still hold, not just the last one.** The
  check previously skipped whenever the incoming import profile differed from the previous call's, on the grounds that a
  differing profile re-imports anyway. Under pooling it does not: a profile served several calls ago can be restored
  without importing, and would be served from `.olean` files rebuilt since. The controller now stamps each profile the
  child may be holding, bounded by the same `runtime.worker_session_pool_capacity` that sizes the child's pool. Cost is
  unchanged at one `stat` per import per call.
- **Opening a worker no longer spends an import proving it can.** Both worker builders opened a session during startup
  purely as validation. This server hands every real call its own imports, so it passed an empty import set to the
  builder and never reused the session that open produced — one full import, one pooled session slot, and one of
  `max_imports`, spent on an environment no call could reach, on every project open and on every per-call semantic
  child. A builder given no imports now opens no session; one given imports still opens eagerly, since then the session
  it prepares is the one later calls reuse. Deployment validation is unchanged and still runs up front, because
  `LeanWorkerHostHandleBuilder::check` — which the server calls when it opens a project — opens a session
  unconditionally.
- **Project-scope `lean_lookup(kind = "references")` parses less instead of remembering more.** Over a 1431-module
  project (76 MB of `.ilean`), a phase split put ~95% of the cost in the JSON parse, so the reader stopped doing the
  parsing it was throwing away: every document was parsed **twice**, because the `version` probe that looked like a
  cheap prefix read actually walked the whole file (Lean's `Json` is an `RBNode`, so `Json.compress` sorts the keys and
  `version` lands last — byte 116,287 of 116,299 in a sampled file); one shared document type served both entry points,
  so a reference query materialized and discarded the whole `decls` subtree while a declaration outline discarded the
  whole `references` subtree; and the reference map was built in full and *then* filtered, at ~400k transient
  allocations corpus-wide. The version gate now runs after a single parse (with the future-version verdict preserved by
  a re-probe on the failure path only), `references` and `decls` have independent projections, and matching entries are
  selected during deserialization so allocation is proportional to hits. Whole-project worst case 427 ms → ~120 ms. The
  reader still holds no cache and no state between calls.
- A `files`-restricted reference query now reads only the indices that can contribute, instead of scanning the whole
  project and filtering afterwards: 271 ms → ~0.5 ms for one file. A path the reader cannot invert with certainty —
  including Lake's mangled filenames, e.g. module `«kan-lint-style»` indexing as `kan-lint-style.ilean` — falls the
  whole request back to the full walk, so a narrowed answer is never smaller than an unnarrowed one.

### Added

- `benches/worker_roundtrip.rs` gains an `inspect_declaration_alternating_imports` arm, the direct measurement of what
  an import switch costs.
- `benches/ilean_reference_scan.rs` measures a project-scope reference query end to end, with a no-hit arm that
  separates the scan floor from result construction and a narrowed arm. The previous number on record — "~565 ms" — was
  a single wall-clock sample from an `#[ignore]`d test on a corpus a third the size.
- A reused session's environment is a snapshot taken at import, so a `lake build` that rewrites an imported `.olean`
  would otherwise keep being answered from the pre-build environment. The controller now stamps the newest `.olean`
  mtime among a session's imports, re-stamps before each reusing call, and recycles the worker when it advances —
  reported as `artifacts_rebuilt` in `runtime.call_restart`. One `stat` per import per call; the whole-build-tree scan
  the obvious alternative implies measured 160–190 ms warm over mathlib4's 8408 `.olean` files.
- `lean_status` reports `broker.lean_max_memory_kib`.
- `benches/cache_hit.rs` measures what a warm `proof_state` on an unmodified file costs — **57.6 µs**, against 21 ms
  when a one-comment edit changes the content hash and forces the elaboration. Nothing covered this: the existing
  `module_query_roundtrip` calls the uncached entry point and measures a full 18–20 ms worker round trip.
- `benches/import_switch.rs` prices an import-profile switch against one resident project: 4.9 ms steady versus 555 ms
  alternating.

### Fixed

- The project-scope reference scan no longer blocks a tokio worker thread. It is file I/O and JSON parsing from first
  byte to last, called straight from an `async fn` with no `spawn_blocking`, so on a large project it stalled the
  runtime for the duration of the scan.
- `telemetry.verbosity = full` now actually puts the telemetry block on the wire. The semantic boundary dropped it at
  `quiet` and then discarded it at `full` too, because `SemanticResponse` had no field to carry it — so the documented
  knob had no observable effect, and every `runtime_*` column in the `smoke_perf` JSONL harness was silently always
  null. Worker generation, restart history, RSS, and the full import list are observable again under `full`; `quiet`
  still omits the key entirely.
- Integration suites that spawn a server now prefer the worker built alongside them over the developer's *installed*
  worker under `~/Library/Application Support`. Without the pin a suite silently exercised whatever binary was installed
  last — which invalidated a worker-memory measurement here, because the installed worker predated the session-reuse fix
  it was supposed to be testing. `stdio_lifecycle` additionally hardcoded `target/debug`, so it resolved nothing at all
  under `cargo test --release`.
- `scripts/memory_stability.py` reported every memory and restart fact as `null`. It read the pre-semantic tool names
  (`inspect_declaration`, `proof_state`, …), looked for the envelope in `structuredContent` when the server's default
  carrier is `content` text, addressed the old `runtime`/`freshness` layout rather than `telemetry.runtime`/`trust`, and
  never asked the server for the telemetry block it was trying to read. A run therefore reported "no restarts" when it
  had measured nothing at all.

## [0.7.0] - 2026-07-24

### Added

- `lean_trial(kind="proof_step")` envelopes are self-contained: every batch result carries the entry goals and local
  hypotheses of the selected proof position (`entry_goals` / `locals`), rendered once per envelope through the existing
  rendering machinery. A proof-stepping trial loop no longer needs a per-step `lean_context` call; boundaries stay
  available as a one-shot navigation call.
- `retry_tainted_non_positive` (default `false`) on `lean_trial` and `lean_verify` opts into one server-side retry of a
  non-positive verdict when the worker was recycled mid-call (tainted by the RSS watchdog). With the flag off, behavior
  is byte-for-byte unchanged: the taint is still reported via `execution_taint` and the existing
  relabel-to-`worker_recycled` policy.
- Batch trial results gain `post_closure_diagnostics`: error-severity entries are moved off `closed` candidates, so a
  `closed` candidate's own diagnostics are error-free and the post-closure consequences (e.g. `no goals` from a
  follow-on tactic) travel in their own bucket.
- By-name verification and the declaration outline now resolve every surface declaration form — multi-clause equation
  `def`s and theorems, `where`-structure defs, `structure`/`class` commands, and anonymous `instance`s under their
  generated `inst…` names — via the lean-rs 0.5.0 shim's declaration-candidate scan repair. `not_found` now means the
  name is genuinely absent.

### Changed

- **Breaking:** `lean_context` drops the declaration echo fields from its default response and makes the boundary list
  and `expected_type` opt-in (`include_boundaries`, `include_expected_type`, both default `false`). The trimmed default
  response is roughly half the old size; set the flags for the old shape.
- Adopted the `lean-rs` 0.5 line (`lean-rs-worker-{child,parent,protocol}` and `lean-toolchain` 0.4 → 0.5) and the
  `lean-semantic-search` 0.5 stack; the supported Lean window is unchanged (`4.26.0 ..= 4.33.0-rc1`).
- Completeness flags that equal their default are omitted from MCP responses instead of serialized (`verified`,
  `truncated: false`, `tainted: false`, zero counts, empty arrays, absent axiom facts), so absent unambiguously means
  "complete / nothing to report".

## [0.6.0] - 2026-07-19

### Changed

- Adopted the `lean-rs` 0.4 line (`lean-rs-worker-{child,parent,protocol}` and `lean-toolchain` 0.3 → 0.4, alongside the
  already-current `lean-semantic-search` 0.4 stack), widening the supported window to `4.26.0 ..= 4.33.0-rc1` via
  `lean-toolchain` 0.4 → `lean-rs-abi::SUPPORTED_TOOLCHAINS`.
- Moved the head toolchain the server is built and tested against from `leanprover/lean4:v4.32.0` to
  `leanprover/lean4:v4.33.0-rc1` (fixture pin, the `src/` head literals, and the README "Versions" matrix / doc JSON
  examples). No Rust-floor change.
- Adapted the `SourceRanges` Lean fixture to the `v4.33.0-rc1` `Environment.addDeclCore` signature, which gained a
  `maxRecDepth : USize` parameter after `maxHeartbeats` (`env.addDeclCore 0 decl none` →
  `env.addDeclCore 0 0 decl none`, mirroring core Lean's own call site).

## [0.5.1] - 2026-07-14

### Changed

- Moved the head toolchain the server is built and tested against from `leanprover/lean4:v4.31.0-rc2` to
  `leanprover/lean4:v4.32.0` (fixture pin and the `src/` head literals), landing on the final release after an
  intermediate stop at `v4.32.0-rc1`. No dependency bump was required: the already-adopted `lean-rs` 0.3 /
  `lean-semantic-search` 0.4 lines (via `lean-toolchain` 0.3 → `lean-rs-abi::SUPPORTED_TOOLCHAINS`) already cover
  `4.32.0`, extending the supported window to `4.26.0 ..= 4.32.0`. Also refreshed the README "Versions" matrix, which
  had lagged at the pre-0.5.0 `lean-rs` 0.2.2 line.
- Bumped the parent crate's `rmcp` dependency from 1.7 to 1.8.

## [0.5.0] - 2026-06-19

### Added

- New semantic MCP surface with five public tools: `lean_context`, `lean_trial`, `lean_verify`, `lean_lookup`, and
  `lean_status`. The surface carries stable `data` / `errors` / `trust` responses, preserving proof-relevant artifact
  freshness facts even when telemetry is quiet.
- `lean_verify` batch target groups for explicit declarations, all declarations in a file or module, and changed-target
  verification. Changed verification reports conservative coverage gaps for unmapped hunks, deleted files, renames, and
  truncation instead of silently dropping edits.
- `lean_lookup(kind = "declarations")` for source-fresh declaration inventory with `.ilean` build-fresh fallback, and
  `lean_lookup(kind = "changed_coverage")` for changed-hunk-to-declaration mapping without verification.
- `lean_trial(kind = "command")` for bounded non-mutating command snippets such as `#check` and `#print axioms`, and
  `lean_status(kind = "file_diagnostics")` for current-source Lean diagnostics.
- Runtime lifecycle hardening for server shutdown, queued-job terminal outcomes, process registry diagnostics, and
  stale-record cleanup that only removes host-owned dead PID records.

### Changed

- Upgraded the `lean-rs` worker crates (`lean-rs-worker-parent` / `-child`, `lean-toolchain`) to 0.3 and the
  `lean-semantic-search` crates to 0.4 (which themselves build on `lean-rs` 0.3), so the entire dependency graph now
  resolves to a single `lean-rs` 0.x line. The Lean toolchain pin remains `leanprover/lean4:v4.31.0-rc2`.
- Public MCP registration is now the semantic five-tool surface. The old six proof-workflow tool names and temporary
  compatibility probes are not advertised; existing internal operation modules are reused behind semantic modes.

### Internal

- Centralized the `lean-rs` and `lean-semantic-search` dependency versions in `[workspace.dependencies]` (members
  reference them via `.workspace = true`) and added a `cargo-deny` CI job whose `deny.toml` floor-bans the `lean-rs`
  crate family below `0.3`. A future partial upgrade that drags a pre-0.3 copy back into the graph now fails
  `cargo deny check` with a named diagnostic instead of a deep `E0308` type mismatch.

## [0.4.1] - 2026-06-09

### Fixed

- `search_for_proof` semantic-lane candidates now return the clean Lean name (e.g.
  `FirstOrder.Language.BoundedFormula.IsDelta0.bdAll`) with the `module` field populated, instead of leaking the
  downstream `origin:module:declName` corpus key as the `name` (e.g.
  `lean-host-mcp:KanProofs.ModelTheory.Delta0.Basic:FirstOrder…bdAll`). The prefixed key broke the documented key-free
  contract and the `search_for_proof` → `inspect_declaration` handoff.

## [0.4.0] - 2026-06-09

### Added

- `search_for_proof` now has a semantic retrieval lane for file/declaration-backed queries. When a request includes
  source context, the tool asks `lean-semantic-search` for proof-goal features, extracts declaration features from the
  candidate modules, ranks semantic candidates, and merges them with the existing declaration-search fallback. Public
  evidence stays key-free: semantic matches surface as stable `semantic:*` `match_reason` labels such as
  `semantic:role_conclusion_const` or `semantic:conclusion_fingerprint`; raw feature keys and capability command names
  stay private. If the semantic command is unavailable or fails, the tool returns the existing structural fallback with
  a warning instead of failing the MCP call. The capability now comes from the package-owned
  `lean-semantic-search-runtime` crate, so consumer projects do not expose or import `LeanSemanticSearch.Capability`.
- Cross-process admission control for semantic/elaborating work. Parallel server processes sharing a lock directory now
  coordinate before running heavy worker calls, so semantic proof search and other elaborating requests do not stampede
  the machine. New `[broker]` knobs and env overrides: `semantic_permits` / `LEAN_HOST_MCP_SEMANTIC_PERMITS`,
  `semantic_waiters` / `LEAN_HOST_MCP_SEMANTIC_WAITERS`, `semantic_admission_timeout_millis` /
  `LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS`, and `semantic_lock_dir` / `LEAN_HOST_MCP_SEMANTIC_LOCK_DIR`.
  Saturation returns retryable structured errors such as `semantic_admission_full` or `semantic_admission_timeout`.

### Changed

- Bumped the `lean-semantic-search-*` crates to 0.3.0 and adapted to the storage-neutral retrieval API
  (`retrieve_across(&[&dyn Corpus], ...)` instead of `SemanticIndex::retrieve(...)`). `lean-semantic-search-runtime` is
  now published, so the host consumes it from crates.io; the temporary local-path `[patch.crates-io]` override is gone.
- Bumped `lean-rs-worker-parent` and `lean-rs-worker-child` to 0.2.0 and `lean-toolchain` to 0.2.1. This pulls link-free
  `lean-rs-abi` metadata into the parent-side dependency graph, so `cargo nextest run --workspace --no-fail-fast` no
  longer builds the parent test binary with an accidental `libleanshared` load command through workspace feature
  unification. The worker crate still links `libleanshared`; the parent crate remains link-free.
- Updated the host for additive `lean-rs` 0.2.0 telemetry fields: RSS/resource fields are ignored where the envelope
  does not yet expose them, declaration inspection leaves the new proof-search facts disabled by default, and module
  query cache-fact fixtures set `resource: None`.

### Fixed

- Workspace-wide nextest no longer aborts while listing parent tests on macOS with
  `Library not loaded: @rpath/libleanshared.dylib`; the parent test binary is again free of Lean runtime linkage when
  built together with the worker.

## [0.3.0] - 2026-06-02

### Changed

- `find_references` at `scope: "project"` now reads Lean's on-disk `.ilean` reference index instead of elaborating every
  `.lean` file through the worker. The old path issued one worker module-query per file (~3 s/file → ~27 min on a
  ~500-file project) and only returned an arbitrary truncated prefix when it hit the request-time budget. The index read
  is sub-second, returns the **complete** result (only the `limit` cap truncates, on a stable sorted prefix), and
  involves the worker not at all. Project-scope results are now **build-fresh** — they reflect the last `lake build` —
  while `file` scope stays on the worker path and remains **edit-fresh** (current source); the asymmetry is documented
  in the tool catalog. An unbuilt project degrades to the existing `needs_build` top-level warning (not an empty "no
  references", not a hard error), and an index stale relative to current source rides a freshness note. The
  `files_scanned`/`files_skipped` counters now report `.ilean` modules indexed / skipped at project scope. The now-moot
  per-file wall-clock scan deadline and its "hit the request time budget" warning are gone.
- The default proof position is now the **pristine entry goal** — the state before any tactic runs. `proof_state` and
  `try_proof_step` previously disagreed: `proof_state`'s `goals_before` showed the entry goal, but `try_proof_step`
  spliced a candidate _after_ the first tactic, so a from-scratch tactic block read off `proof_state` failed with
  `introN failed: ... no additional binders to introduce`. Now `proof_state` at the default reports the entry goal
  (`goals_before == goals_after`) and a default `try_proof_step` snippet elaborates against that same goal, so
  from-scratch blocks work at the default. The old first-tactic state stays reachable as `{kind:"index","index":0}`.
  **Behavioral change** for callers that relied on the default mapping to the post-first-tactic state.
- Bumped the `lean-rs-worker-parent` / `-child` and `lean-toolchain` dependencies to 0.1.20, which adds the upstream
  `Entry` proof-position selector this reconciliation maps the default onto.

### Added

- For explicit `{kind:"index"}` / `after_text` positions, a failed candidate carrying a binder-introduction diagnostic
  now surfaces a cue pointing at the entry default (or continuing from `goals_after`), so the trap is signposted even
  off the default path.
- Worker provenance now records the building `lean-host-mcp` version. Worker and host are version-locked, so a worker
  built by a different host is flagged as stale — closing a skew that previously served an ABI/protocol-mismatched
  worker silently (it would fail at call time instead of with a clear message). `install-worker --list` gains a `host`
  column (`current` / `stale` / `unknown`), and a project served by a host-skewed worker now rides a rebuild warning in
  every envelope.
- `install-worker --auto` (the default) now rebuilds **stale** workers — host-version skew, `lean.h` header drift, or a
  failed/absent smoke record — not just missing ones, and skips out-of-window toolchains instead of failing on them. So
  re-running it after a `lean-host-mcp` upgrade brings every worker back in step. `--force` rebuilds current workers
  too.
- `install-worker --clean [--toolchain <id>]` removes all installed workers (or one); `install-worker --prune` removes
  only unservable workers (outside the supported window, or with a failed smoke test), keeping servable-but-stale ones.
  Both are idempotent and only touch the install root.
- Configurable per-request timeout: `runtime.request_timeout_millis` (env `LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS`),
  default **120 s**. Replaces the worker's fixed 10-minute long-running profile. On expiry the worker is recycled and
  the call returns a retryable runtime error; raise it for unusually heavy modules, lower it to bound calls.
- `find_references` at project scope is now bounded by that same budget as an overall wall-clock deadline, not just per
  file. It fans one worker query out across every module in the project (hundreds in a large tree), so the per-request
  timeout alone left the aggregate sweep able to run for many minutes and appear to hang. The deadline now runs
  concurrently with each in-flight query, so even a single stalled module cannot block the call; whatever was indexed
  before the deadline is returned with a truncation warning and a cue to narrow with `files` or raise the budget.

## [0.2.0] - 2026-06-02

### Changed

- Tools no longer advertise an `outputSchema`. Handlers return a bare `CallToolResult`, so `tools/list` carries no
  nested `$defs` (~52 KB → ~9.6 KB). The Anthropic Messages API dropped the field before the model anyway, and deep
  `$defs` broke strict clients (Claude Desktop, Zed) — proof agents read the JSON envelope as text either way.
  **Breaking** for any client that validated tool responses against the advertised schema.
- Per-call telemetry is now config-gated and omitted by default. `Freshness` splits into the always-emitted
  `FreshnessIdentity` (`project_root`, `session_id`, `lean_toolchain`) and an optional `Telemetry` block
  (`project_hash`, the full `imports` list, worker `RuntimeFacts`) that is dropped under the new default
  `telemetry.verbosity = quiet`; set `full` to restore today's output. `proof_state`'s `query_facts` and
  `search_for_proof`'s search funnel (stage counts, cache status) likewise appear only under `full`. The one actionable
  signal a worker restart carries still surfaces as a top-level `warning`.
- New `server.response_carrier` knob (`text` default, `structured`, `both`) selects whether the JSON envelope rides in
  `content` text, `structuredContent`, or both, instead of always duplicating into `structuredContent`.

### Removed

- The per-call tuning knobs `max_field_bytes`, `max_total_bytes`, and `heartbeat_limit` left the `inspect_declaration`,
  `try_proof_step`, and `verify_declaration` request schemas; they now live in `[output]` server config with the same
  defaults. **Breaking** for callers that set them per request — configure them server-side instead.

## [0.1.0] - 2026-06-01

### Added

- `cargo install lean-host-mcp` now works without a source checkout. When the server binary was not built from a
  checkout, `install-worker` builds each per-toolchain worker from the published `lean-host-mcp-worker` crate
  (`cargo install lean-host-mcp-worker --version =<ver>`) instead of erroring; from a checkout it still builds the
  worker from workspace source, and `--source-dir` overrides the choice. The worker is still compiled locally per
  toolchain (its rpath is machine-specific) and smoke-tested before use. Both crates are now published to crates.io.
- Unified TOML config file for every tunable knob. A `lean-host-mcp.toml` (found by walking up from the working
  directory, like the lakefile) or the home `~/.config/lean-host-mcp/config.toml` can set the `[runtime]`, `[broker]`,
  and `[server]` knobs that were previously env-var-only, plus the existing `primary_project`. When both files exist
  they merge per key (local wins); precedence is `CLI > env var > file > built-in default`, so existing
  `LEAN_HOST_MCP_*` setups are unaffected and an env var still overrides the file. Malformed files are logged and
  ignored. See [docs/operations.md](docs/operations.md#configuration-file).
- `lean-host-mcp config init` writes a documented starter config file — every option at its current default, each with a
  comment explaining it — to `./lean-host-mcp.toml` (or `~/.config/lean-host-mcp/config.toml` with `--home`, or a
  `--path`). The file, the per-knob reference table in the docs, and the built-in defaults are all generated from one
  in-code catalogue, so they cannot drift.
- Worker-recycle observability: each recycle is now logged to stderr with structured fields (`cause`, `reason`,
  `worker_generation`, `rss_kib`, `limit_kib`, `restarts_total`) at a signal-appropriate level (`warn` for
  abnormal/crash, `info` for memory-pressure cycles, `debug` for hygiene), and every response's `runtime` carries
  lifetime `restarts_total` plus a per-cause `restarts_by_cause` breakdown so recycle *frequency* is visible. See
  [docs/operations.md](docs/operations.md#observing-worker-recycles).
- Structured `tracing` across the server's high-value paths (tool entry, project open/eviction, the idle reaper, the
  per-call job span, RSS headroom, toolchain resolution, and verdict-relabel decisions), all on stderr so the stdio
  transport's stdout stays clean. Default level is `info`; `RUST_LOG=lean_host_mcp=debug` surfaces per-call detail.
- RSS-config guard rails: the server validates `import_switch <= post_job <= hard_kill` at startup and refuses to start
  with a clear `invalid RSS config: …` message on an inverted ordering, so e.g. raising
  `LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB` above the hard-kill ceiling fails fast instead of degrading silently.
- Honest `worker_recycled` verdict: when the worker is recycled or restarted *during* a semantic call (a memory-pressure
  recycle on a heavy module, or a crash-and-retry), the verdict was computed under infrastructure duress.
  `verify_declaration` now relabels a non-positive verdict to `verification_status: "worker_recycled"` with
  `facts_trustworthy: false` instead of a misleading `not_found`, and `try_proof_step` / `proof_state` carry a retry
  warning. A `verified` verdict is never relabeled (verification is monotone). The signal is derived from the call's
  runtime facts (`call_restart`) and excludes benign pre-job/planned cycles.
- Initial release of `lean-host-mcp`, an MCP server that hosts Lean 4 in a supervised worker child
  (`lean-rs-worker-parent` + `lean-rs-worker-child`) and reaches the elaborator and kernel directly rather than through
  an external LSP.
- Two-crate workspace: a parent that does **not** link `libleanshared` and a per-toolchain worker binary that does —
  keeping the parent free of the Lean dylib so one server can host multiple toolchains.
- Multi-toolchain dispatch: each Lake project resolves its own `lean-toolchain` pin to a worker binary under
  `~/.local/share/lean-host-mcp/workers/<id>/`; `install-worker` subcommand builds and installs them.
- A six-tool declaration-centric proof workflow:
  `proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration`, plus
  `find_references` for semantic lookup. `proof_state` degrades to `{ "status": "unsupported" }` when the optional host
  shim is absent.
- Closure-channel actor over the worker child, with a `ProjectBroker` per-project pool and idle reaper.
- Stdio (default) and loopback-only Streamable HTTP transports.
- Response envelope contract (`result` + `freshness` + optional `warnings`/`next_actions`) shared by every tool;
  Lean-domain failures are part of the `Ok` payload, not MCP errors.
- Worker RSS supervision: a post-job restart policy and an in-flight hard-kill watchdog, plus the
  `rss_threshold_sweep.py` tuning tool.
- Honest resolution verdicts: an incomplete project build degrades to a single `needs_build` verdict carrying a
  `lake build` cue across `verify_declaration`, `inspect_declaration`, `proof_state`, `try_proof_step`,
  `find_references`, and `search_for_proof` — never a misleading `ambiguous` status or a hard transport error. Genuine
  ambiguity instead names the competing declarations, and `facts_trustworthy` flags any verdict computed against an
  incomplete or unresolved environment.
- Builds on `lean-rs-worker-parent` / `-child` 0.1.19 (worker protocol 8), supporting the Lean toolchain window
  `4.26.0 ..= 4.31.0-rc1` (head `4.31.0-rc1`).

### Notes

- Pre-1.0: minor versions may carry breaking changes; patch releases stay compatible.

[Unreleased]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jcreinhold/lean-host-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jcreinhold/lean-host-mcp/releases/tag/v0.1.0
