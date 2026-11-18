# Native execution progress

## Current priority and checkpoints: 2026-11-18 13:51 UTC

The active plan is **4.36 upgrade first**, then lifecycle redesign. Major overhaul implementation and new test/build scheduling are paused after safe checkpoints while the minimal compatibility split is finalized. Preserve all patches; do not revert or discard work. ABI owner6afca3dd owns minimal SDK compatibility/safety ancestry; installer6afca1a9 owns isolated upgrade-only host route analysis. Semantic/dup remain with lean_version; formatter is already complete. Recovery/deadline/service/caller owners are paused, not silently abandoned. Already-running focused compilation may finish safely; no new suites.

Upgrade-only dependency correction: semantic must start from released v0.7.7 `55832816a7b0857ca3d3aa7fe6c3e595f31682a4`, port Lean delta `31cc612937767191748b395ad447d35897244a29` and packaged sync `f5cdb01c339047dd5b098a3f00c2300105c4d43f`, then regenerate metadata. The later `f1202a4`/`667e452` Rust line already inherits lifecycle custody/writer APIs and cannot be called upgrade-only. ABI owner is selecting a narrow SDK preserving released v0.9.9 materialization/build/signature APIs plus necessary 4.36 and public ABI safety repairs. No qualified minimal SDK exists yet.

Release provenance gate: historical disposable Cargo overrides and registry-named lock entries without checksums are NOT proof of a published pair. Before release, verify exact tested SDK/semantic versions resolve from the intended published source and generate genuine registry lock checksums without local patches. Check present registry availability at that point; do not infer it from old audit results.

Host upgrade-only baseline verified at 13:53: released `v0.16.7`, commit `994d37ddddee9af1c434c69b176387ccc784c1b9`, clean isolated `task-15/host-upgrade-436.EFhK8j`. Minimum expected files: root Cargo manifest/lock, `fixtures/lean/lean-toolchain`, README support matrix. Released installer builds/fetches matched worker, runs Init smoke and readiness sidecar without the later guardian redesign. Exact SDK/semantic pair, compiled-header/worker-RPATH/runtime prefix agreement, and isolated official-name toolchain discovery still need qualification. Owner preparing source/config while SDK selection proceeds; no build started. Existing private guardian fixture passes do not qualify this released route.

Narrow SDK source status, 13:58 UTC: released-base tree `2b26aea4fab4d9c48c08a5c3201428b6bd3fbcd7` on v0.9.9 `f3c3800937e4a94ac75aa0208b002dc0a709e927`; cumulative patch `9ffb2068f496e687ac72e190898e376b33be431205c386794d14ffe200ec1209`. Independent review clears scoped sys/toolchain/lean-rs development compilation, not release. It preserves released materialization/build/signature helper files but makes unchecked native load/bind/call/raw decoding boundaries source-breaking; this is a safety backport, not merely version registration. ABI owner is adapting the legacy host/child load boundary without adding lifecycle authority. Minimal necessary safety scope and SemVer disposition are under explicit review; no final SDK version/publish approval is inferred from prior0.9.10 plans. Do not ship known unsound safe APIs to avoid a breaking version.

Host focused prefix probe verified at 14:03 UTC: one isolated dependency-free run exited0 in3.45s, emitted both selected RPATHs and reran on prefix change despite conflicting toolchain selector, rejected missing runtime/header and relative path. Private one-job Cargo home/build/target only;776KiB growth, no Lean load/shared target. Evidence `task-15/host-436-runtime.DNuu4t/prefix-regression-v2.log`. Host constructor test and actual installed worker remain blocked on narrow SDK adapter; do not infer runtime qualification. SDK owner remains active on that adapter; scoped compile results have been requested and are not yet verified.

Actual SDK development result: core `lean-rs-sys`, `lean-toolchain`, `lean-rs` check passed on4.36 at14:06:20–14:06:22 UTC, PID57453, exit0,26.5MiB growth. Tree `2b26aea4`, archive `03b6ae162d19f6b635d32cc0a5428655e839a5f4e38a385019343d0aedad3da0`. Build warned zero fixture digest, so this establishes compilation only. Legacy host/child adapter `d09dee8b` independently cleared for scoped compile and execution admitted turn `01a1eefb-c62b-744c-9c1b-c61e887fbacc`; no runtime or final release version yet. Native export trust and requested initializer correspondence remain explicit adapter obligations. SDK0.10 recommendation requires downstream dependency-bound changes; do not publish under old0.9-compatible assumptions.

Adapter milestone verified after the 14:08 owner callback: released host and worker-child library checks passed in 2.66 seconds, then 1.43 seconds after actual-library/requested-initializer binding. Final adapter patch SHA `ae90f71aaba7d87abf19dee87dd053cf3f43d98f6fd8a26044324e5edde4daf9`; log `task-20/upgrade-only-sdk.lg129tvr/adapter-check-2.stderr`. Export approval explicitly covers every protocol-valid payload for that export, not per-payload authorization. Initializer-mismatch regression remains unrun; zero fixture digest and bundled shim/runtime qualification remain open. SDK owner started turn `01a1eefe-f127-7380-b237-a70b8d3e9272` for focused regression and real fixture/shim prerequisites, with a sole two-job warm-target lane, 1 GiB growth cap and 25 GiB disk floor. Admission is verified; command execution/result is pending. Final coherent snapshot and version are requested before downstream graph qualification. No full suite or lifecycle work resumed.

SDK prerequisites completed: 14:11:19–29 UTC fixture build and 14:11:29–36 shim build exited 0; initializer regression passed 1/1 at 14:12:50–51; host/child check passed at 14:13:16–18 with nonzero fixture fingerprint `880c16397d19d21d472d6b797501200ffdfaed2f491a3cd9e3d266b444ed42fe`. Frozen source archive `17e4a2a3bafd6d87137e8be83bacdf0fa7f191a06f9ad38213290a44814fd5a1`, tree `a909db2f6b37dabf83880cdf9be927cfea465a36`. SDK owner turn `01a1ef02-4588-732b-b49d-7a5092b259e1` now owns legacy test migration and synchronized 0.10.0 metadata freeze, with the sole bounded focused compiler lane. Host owner turn `01a1ef02-3404-7177-a813-4cabefa7945d` owns released-host WorkerAuthority/initializer wiring and matched smoke preparation, source-only until lane allocation. Native shim RPATH closure and actual legacy runtime remain gates; neither is inferred from compilation. Both turns admitted started; execution results pending.

Native integration blocker, verified at 14:18 UTC: both SDK and released-host owners confirm startup manifests and existing stale-worker cache do not protect transitive imports/plugins/sidecars from replacement during later Lean use. A permissive WorkerAuthority or outer unsafe wrapper does not satisfy this condition. Actual released host entry point remains unchanged. The SDK candidate now has synchronized 0.10.0 metadata and migrated test targets (archive `ee35aa41f4486457bbebb8a2af38217dbfeb5869758af07a6f77bc058a4ad7a7`, tree `1a9a020f8136e71b6b178888b2c4849c99ef91fb`); host targets typechecked and initializer regression passed 1/1 at 14:17:13. Production runtime is blocked on a concrete stable-input snapshot or sufficient cooperating writer exclusion, not the whole paused lifecycle design. Host owner is proposing the smallest reuse-based implementation; independent reviewer checks necessity and preserved guarantees. No native qualification/publication inferred.

Verified separation: canonical SDK→`750846a` has three commits: `ada6690` fail-closed compatibility probes, `e9baea0` complete raw export coverage, `750846a` removal of two unavailable string exports. This is NOT yet a complete or released 4.36 candidate. Public ABI safety repairs, supported metadata/allocator/runtime evidence and version reconciliation require explicit selection. Host `355f1e8` is only migration documentation, not implementation. Minimal upgrade must retain current production paths unless a concrete incompatibility requires change, without importing durable service/recovery redesign by default.

| Overhaul owner | Verified saved checkpoint | State and strict upgrade relevance |
|---|---|---|
| Recovery `6afdaaea` | `task-3/sdk-recovery.patch`, SHA `01b9102724247f3ce1736af0d158e421fea49e93a3985884c32054aba7376594` | Paused, six files, formatting/apply only; no build/native custody. Owner confirms not required for4.36. |
| Service `6afdaad5` | `task/native-service-source.patch`, SHA `af8041b2db7f1e9a51903588aac3ca4321e5c49e479029be62b11e851a4768cc`; registration `104c11d718d7df8e32e67ea64a67a4062afb0342d21a702c3e6c27be8fde770e` | Paused, eight new modules/schema files, apply/format only, no compile/runtime. Discovery intentionally empty and execution disabled. Not required for4.36. |
| Callers `6afdaaf5` | `task-4/proofs-native-execution-callers.patch`, SHA `bb9b9988f78198f7901b0763c5158f42716eeaaba22f5b7f3fa90d0c3ef6be8d`, isolated base `bc2c4ca264480c948cfdf55aa64fd2ade6ae91da` | Paused, uncommitted; seven Python tests passed before final lint rename. Existing callsites not yet migrated. No Lean/Mathlib/native service; canonical Proofs clean. Not required for4.36. |
| Deadline `6afdaae0` | Original `e96561e64fa9352d766a51e4511d68807fa94b176bbdaddf917bf30469f74812`; sender draft `0ffd3dd685fe101eaed269f893ce32bc05666c399200a08dc8f0cc32bcb86f7b`, task-2 | Paused, no process/custody, canonical SDK clean. Neither patch required for4.36. |
| Installer `6afca1a9` | Nov17 task-15 production-bootstrap-actor-selection.patch `474318382d13276da309d967fe3c52152730e4cfb046678a9370b3df46064b7d` | Overhaul paused, no build/test active; existing MCP server89108 untouched. Five files are lifecycle/provisioning only, not version/ABI/loader changes. Owner reassigned isolated upgrade-only host analysis. |

New-owner paths above are under `/Users/jcreinhold/Documents/Codex/2026-11-18/`; installer is under the explicitly noted November17 task-15 path. New source changes remain isolated; canonical SDK/host heads are unchanged. Earlier implementation ownership/requirements below remain the saved overhaul plan, not authorization to resume expansion while the split is being decided.

## Priority update: 2026-11-18 13:44 UTC (superseded by 4.36-first priority)

Finish **lean-rs upstream first**. ABI composition owner and SDK deadline/recovery owners are actively coordinating a coherent SDK API/implementation with focused development checks. Host installer, service and caller implementation continue independently; downstream fixture convenience is not a prerequisite for SDK publication. Genuine SDK safety/API dependencies remain required. No full suites before implementation is ready.

Canonical SDK manifests contain no `lean-host` dependency. External writer-helper pairing is a runtime integration requirement, not a reverse Cargo dependency. Current SDK publish script order is ABI → sys → toolchain → interop-shims → lean-rs → host → worker-protocol → worker-parent → worker-child; revalidate against final candidate metadata before release.

Installer's five-file controlled-build plus multi-project-selection patch is applied to a private copy of the frozen production archive; all 831 input hashes and forward/reverse applicability verified, syntax passed. It refuses unselected roots/toolchains before acquisition. Compilation/regressions remain pending. Shared compiler priority belongs to focused SDK work, with host source review proceeding concurrently.

## Five SDK API work items: tracked plan, 2026-11-18 14:35 UTC

**Priority remains 4.36 first.** This section records requested future implementation and acceptance, not authorization to restart paused redesign. API names below are proposed. Coordinator: `/root/overhaul_check`; SDK accountable owner: thread `6afca3dd-61c8-8191-beef-0e1235e410d2`; host integration owner: `6afca1a9-4f1c-8191-a757-cb2c906df0dc`. Independent source reviewer: `warm_paired_review`; implementation must receive a fresh review after changes. Owners are identified, but these five design items are **not active implementation assignments**. Assignment/start times for future implementation are unset, not inferred from old session age.

| ID / deliverable | Concrete files and dependencies | State / next decision | Required positive and negative acceptance evidence |
| --- | --- | --- | --- |
| SDK-1 Prepared project/generation handle | Proposed facade in `lean-rs-host`; compose existing `crates/lean-rs-host/src/host/lake.rs`, `host/session.rs`, `crates/lean-rs-worker-child/src/child.rs` HostAnchor. Depends on SDK-2 contract and SDK-3 input mechanism. | Planned, not implemented. SDK owner selects exact public module with host owner. Handle binds runtime, actual library/initializer, ordered roots, generation identity and lifetime owner; logical source paths remain separate. | Compile and run ordinary project prepare/spawn/query with the actual released host; reject wrong runtime, initializer and reordered roots; missing imports produce explicit NeedsBuild; old generation cannot silently resolve new native outputs. Record exact source/artifact hashes and test names. |
| SDK-2 Honest native safety boundary | `crates/lean-rs/src/module/library.rs`, `module/trusted.rs`; `crates/lean-rs-worker-child/src/authority.rs`; host worker `crates/lean-host-mcp-worker/src/main.rs`. Precedes SDK-1/3. | Partial low-level implementation in isolated 0.10.0 candidate, not complete host integration. Library/initializer pairing and explicit unsafe attestation exist; ordinary host cannot yet satisfy current custody contract. Next: decide minimal compatibility/lifetime contract without permissive authority. | Independently review every unsafe constructor/call path and ABI/ownership claim; wrong initializer/export contract refused; manifest A/image B substitution regression; reachable values/global registrations never outlive native code. Demonstrate which conditions are enforced and which are trusted application preconditions. Hash/typecheck alone is insufficient. |
| SDK-3 Practical input compatibility with Lake | `crates/lean-toolchain/src/source_package.rs`, SDK host `lake.rs`/`session.rs`, worker `child.rs`; host `lake_meta.rs`, `project.rs`, `cache.rs`. Depends on SDK-2, developed with SDK-1. | Unresolved mechanism, no implementation admitted. Stable compatible build generations, complete cooperating writer discipline, or snapshots are alternatives; whole-project snapshots are not intrinsically required. | Actual concurrent replacement and deferred import tests must not execute mixed incompatible ABI generations; unrelated source/output edits remain allowed where harmless. External stock Lake does not honor an SDK-only lock. Select isolation or an explicit stable-output policy, and reject unsupported mutation rather than claim coverage. Test transitive native/olean resolution, partial build/error, path dependencies and generation refresh. Pre/post hashing alone is not a consistency proof. |
| SDK-4 Supervised worker facade | `crates/lean-rs-worker-parent/src/supervisor.rs`, `crates/lean-rs-worker-protocol/src/types.rs`, child dispatch; compose SDK-1 rather than duplicate supervisor. | Existing typed requests and shutdown machinery; prepared-generation binding and complete public workflow unfinished. Future SDK owner, host owns scheduling policy. | Reuse and successful request; timeout, explicit cancel, crash, stale response and failed retirement; actual child/group closure evidence before releasing corresponding resources. Preserve first execution uncertainty and distinguish cleanup-pending from complete closure. Inject partial-frame writes and lost replies; assert no automatic replay and use a duplicate-dispatch oracle to prove the operation was not sent again. Guardian/frozen-owner/service-restart guarantees remain separate paused lifecycle gates, not implied by these tests. |
| SDK-5 Small public surface and explicit trust rules | Public exports/docs/examples in `lean-rs-host` and worker-parent; advanced adapter in `worker-child/authority.rs`; host `project.rs`, `broker.rs` and worker main. Depends on SDK-1 through SDK-4. | Planned facade such as prepare → spawn → query/verify → close, not existing symbols. Everyday clients should not implement every per-operation authority callback. | Compile examples and migrate real host/semantic consumers without duplicated authority/lifetime logic. Review explicit native-code/export payload trust and supported external-writer model. Invalid input, unsupported profile and incomplete evidence must remain errors. Verify source-breaking version/migration docs and exact published dependency resolution. |

### Evidence and completion rules for all five

- Current source anchor: isolated SDK tree `1a9a020f8136e71b6b178888b2c4849c99ef91fb`, archive SHA `ee35aa41f4486457bbebb8a2af38217dbfeb5869758af07a6f77bc058a4ad7a7`, synchronized 0.10.0 metadata. Host targets typechecked at 14:16:38; initializer regression passed 1/1 at 14:17:13. These establish only those bounded facts, not completion of any row.
- Each row moves through **planned → implemented → focused tests passed → independently reviewed → integrated consumer verified**. Save assigned/start UTC, last concrete progress UTC, immutable patch/commit, exact commands/test names/counts and logs, negative oracle, reviewer result, remaining limitations and next checkpoint. No percentage-complete or guarantee from a passing fixture. Coordinator updates this same ledger when evidence changes.
- Review the realistic ABI counterexample: trusted build B replaces a same-named native library after manifest A is read; the export still resolves but has different argument/result representation or ownership. Missing files/symbols can be recoverable errors; an incompatible successful call can be undefined behavior. This is a plausible counterexample, not an observed incident. Whole-tree bit identity is stronger than the necessary compatibility/lifetime guarantee.
- Proof verification is distinct from semantic ranking or successful FFI. Existing protocol verification facts include target, diagnostics, unresolved goals, sorry/admit/sorryAx, axioms and availability/truncation. Bind verification to the exact source/target/runtime/import generation and apply repository acceptance policy; unavailable axioms are unknown, not an empty axiom set. Required cases: valid theorem, false candidate, sorry/admit, disallowed axioms, missing imports, truncated evidence and timeout. Kernel/Lean implementation must be inspected and tested before claiming that complete route qualified.
- SDK owns reusable loading, artifact lifetime and typed worker mechanisms. Host owns project permission, global queue/capacity, durable jobs/history and installation. Do not absorb the paused SQLite/deadline/recovery/caller work into a 4.36 compatibility patch without a separate explicit scope decision.

Current capacity: SDK compile lane released after the 14:17:13 focused test. Confirmed to capacity and lean_version at the 14:33 request; subsequent boundary work is source-only. Semantic bounded Rust qualification may use its separately allocated lane.

Downstream update: semantic narrow tests passed during 14:34:51–14:36:06 UTC per its coordinator, and its compiler lane is released. This does not qualify legacy native loading or settle the ordinary-project trust boundary. Runtime/release remain held pending a justified implementation contract and actual matched native execution.

Actual controlled scalar native result verified: 14:42:08.694–14:42:11.659 UTC, PID 65759, narrow SDK worker 0.10.0/protocol 12 returned U64(42) for CallFixtureMul(6,7), acknowledged termination and exited 0 without timeout. Original frames and single-run.json read under `task-20/upgrade-only-sdk.lg129tvr/controlled-scalar/`. Worker SHA `9ffc19274d7e4ba75c2586090fad619b97fadc2b42d7d276bbd9267eb3a57e8d`. This is controlled scalar/initializer evidence only. Private directory permissions and retained file handles do not establish adversarial same-user mutation exclusion. Independent review requested; host-import/semantic and ordinary mutable-project gates remain open.

Controlled host-shim runtime verified from original frames and receipt: worker 0.10.0/protocol12 imported Init (668 effective modules, 66,150 constants), returned theorem for Nat.add_zero, acknowledged termination and exited 0/no timeout, PID66356, 14:46:42.655–14:46:48.980 UTC. Evidence `task-20/upgrade-only-sdk.lg129tvr/controlled-init/`; worker SHA `f135d748e93bb2e86f3f6b4ddd151939b9dbbcfceb523d6dae0d851a5094520e`. Independent review active. This relies on controlled selected artifacts/no writers; it does not resolve general mutable-project support. SDK source unchanged, compile lane released. Next coordinate actual narrow semantic capability load/cold-warm without reusing overhaul-only authority.

Main integration verified at 15:30 UTC: canonical SDK main is clean at `b0bbee957dcda64ab9e5c68a180a858ac088d3b4`, preserving both canonical `a24394e` and released-base `60c922` through merge `a7cbcb5`. Merged library check and all 14 compile-fail cases plus positive executable passed; independent review cleared the 27-line obsolete-safe-loader deletion. Source archive SHA `9f575a1b8c475566c1d175febf69b98a7ff71e3fc75777c5c934e8014b14632f`. Nothing pushed/published. Canonical host prefix fix is `25e54dd`; progress ledger history committed in `0febb9b`. Merged protocol13 native qualification remains open; earlier protocol12 receipts cannot substitute.

Latest host source composition: `task-15/host-narrow-composition.F7bQWa`, released host v0.16.7 + reviewed prefix patch + SDK `ee35aa41` + semantic `da9e813e142b2eec84568d740f197e602442b945`; 470 SDK file hashes verified. Private overlay selects =0.10.0/=0.7.8, baseline lock unchanged pending resolution. No host build/native load; ordinary-worker authority remains unresolved.

## Saved overhaul implementation dependency plan: 2026-11-18 13:37 UTC

Production implementation is the critical path. Nonessential Tiny parity/expiry fixture builds and full suites are paused; focused development compilation/regressions remain allowed. No deployment or release completeness is claimed.

| Task and exact repository files | Owner / state | Dependency and completion condition |
|---|---|---|
| `lean-host-mcp`: `crates/lean-host-mcp/src/cli/install_worker.rs`, `crates/lean-host-mcp/src/cli/install_worker/release_recipe.rs`, `crates/lean-host-mcp/src/cli/install_worker/release_recipe/install_inputs.rs` | Installer session `6afca1a9-4f1c-8191-a757-cb2c906df0dc`, active production turn `01a1eedf-8611-706b-ac51-a22d332091a8`. Three-file controlled-install patch exists, parses and applies; not compiled. | Wire owned verified controlled-build output into existing guarded install; preserve custody and protected selection. Focused wrong/crossed destination and existing-selector tests, then actual controlled output must match independently selected digest. Reproducible output is unresolved. |
| `lean-rs`: `crates/lean-rs-worker-child/src/authority.rs`, `crates/lean-rs-worker-child/src/child.rs`; host `crates/lean-host-stock-init/src/project_policy.rs` | ABI session `6afca3dd-61c8-8191-beef-0e1235e410d2`, active turn `01a1eedc-242c-7700-8b64-675ecab61036`. Patch `c2fcc6c0dc9d1b40cf225ba5f6178a0f59cf386163ebc3ff245d9aae689ca52b` source-reviewed; uncompiled. | One-use confirmation after existing guardian role/digest comparison. Focused before-confirmation, crossed role/digest, duplicate, error preservation and consumed-grant tests. No new authority system. |
| Host `crates/lean-host-stock-init/src/project_selection.rs` and related selected import/inventory policy | ABI owner, implementation underway; current fixed-pin dependencies remain. | Replace fixed record/import/builtin/elaborator pins coherently with authenticated record-associated inputs. Depends on preceding first-open transition and installer-selected records; retain complete toolchain, allocator, loader, epoch and import closure checks. |
| Host `crates/lean-host-mcp/src/main.rs`, `broker.rs`, `native_reader/managed.rs`, `native_reader/managed/project_build.rs` | Installer owner assigned normal authorized-project selection; not complete. | Bind canonical project/toolchain to independently selected main/companion receipts and allowed operations; publish under actual writer custody and retire/reacquire on epoch change. Depends on dynamic worker policy. No self-authorizing request digest. |
| Final SDK/host dependency assembly | ABI owner, pending production changes above; downstream coordinator owns semantic/dup updates. | Current combined archive `6843cec050d6220d7026baabc96e1cd63a31d74f22ca056639e79c12a7934814` still uses Rust semantic `c30d8b3d2001fbb7d7e28e9e74217ae711ea4d14`, not portable `667e45232e84259fb687574e6dbad7188389a501`. Integrate reviewed final dependencies before final suites/publication. No final release SHA yet. |
| Integrated acceptance and release | Starts after implementation and dependency assembly, not before. | Two authorized projects, crossed/missing approvals, stale/rebuilt epochs, ordinary main/companion routes, cancellation/expiry/owner loss and custody closure; then unpatched dependency/package/platform release gates and ordered publication. Reuse retained evidence only for unchanged inputs. |

### Saved parallel implementation ownership, now paused; verified 13:42 UTC

Four new durable Mac sessions were admitted `started` during 13:40–13:41 UTC, explicitly selecting `gpt-6.2-sol`. Exact OS process start is not inferred. All are source implementation in isolated workspaces; no compiler lane or full suite is allocated.

| Work | Session / initial turn | Exclusive files and dependency |
|---|---|---|
| Durable service/store/reconnect | `6afdaad5-ec08-8191-a961-ce3c72fcd0ca` / `01a1eee3-5e35-7150-b05b-4f16e354370f` | Proposed host `native_service/` namespace and focused tests; existing contract reuse, minimal client interface coordinated with caller owner. Production dispatch depends on custody integration. |
| Independent deadlines/activity | `6afdaae0-3ca0-8191-8441-3704f2f580c5` / `01a1eee3-878e-76e9-8079-b63854d2d700` | Proposed SDK `native_guardian/deadlines.rs` and policy/accounting tests; reuse prior kernel. Main guardian hooks coordinated with recovery owner, not concurrent edits. |
| Frozen/dead owner recovery | `6afdaaea-745c-8191-bf82-403f7f4040c0` / `01a1eee3-aef1-76ac-8fec-c7785a32af31` | SDK guardian ownership/recovery and record-lifetime tests in isolated overlay; existing registry/token/boot machinery, no new authority. No native fault run. |
| Caller migration/removal | `6afdaaf5-a66c-8191-b2db-b4d11426d0da` / `01a1eee3-d9c7-77f2-b464-c172c6f4ec7b` | Isolated Proofs Python writer/reader/artifact adapters and orchestration; small service client contract with service owner. Text/Python-only development checks; no Lean/Mathlib execution. |

Existing ABI owner remains sole worker-child/stock-policy and final composition owner, active turn `01a1eee4-ebbe-72ec-b6ab-398cbc2b4ffa` admitted by 13:42:22. Installer owner remains sole install and normal host project-selection owner. Shared registration/Cargo changes are proposed patches for composition, not concurrent canonical writes. The earlier unassigned labels below describe the 13:38 audit and are superseded by this ownership table, not by completed implementation.

New immutable production source archive: `027b6b58073044a6a7c11cfbfa09e214af59446f152cada0a2fce0475ef5eb7d`; 831 files reverified, manifest `0baf39e9bab9a3aeae288667b1a4ff746beb589cfb0c1949847541591466dbc9`. All five semantic patch paths now point to immutable `667e452`; four resolve in host graph, unused store independently verified. Installer received this archive. This supersedes the earlier mutable semantic assembly gap only; focused compilation, production integration and lifecycle gates remain unfinished.

### Whole-overhaul requirements not completed by the current production slices

Audit at 13:38 UTC against design §16 and canonical code. These remain **required and unverified**, not silently deferred or declared complete. Implementation start times are unknown unless explicitly listed; a source-reviewed prototype is not a production gate.

| Gate / exact existing boundary | State, owner and last evidence | Dependency / next implementation |
|---|---|---|
| Durable native jobs, reconnect and caller-save-before-ACK: host `crates/lean-host-mcp/src/{server,tasks,request_journal,request_log}.rs`; private contract `specs/native-execution/stage-store-contract.md` | Production owner not yet assigned; coordinator must allocate a distinct lane. Canonical `request_journal.rs` explicitly says atomic snapshots are evidence, not a replay queue. `crates/lean-host-mcp/Cargo.toml` lists SQLite only as Unix dev-dependency for unwired contract tests. Last audit 13:38; production start unknown. | Design §16.4: choose production service/store/protocol placement, wire authenticated original-ID lookup, one-use GO and persisted caller binding before ACK, retention/corruption/restore/asset publication. Depends on qualified registry admission and warm lifecycle; existing semantic tickets must not be reinterpreted as native attempts. |
| Independent hard deadline and live activity: host `crates/lean-host-mcp/src/{broker,project}.rs`, `broker/control.rs`; SDK `crates/lean-toolchain/src/native_guardian.rs` | Production integration owner not assigned. Existing startup/operation timeout helpers and pool activity counts are not §7 CPU/checkpoint accounting or independent hard-bound enforcement. Earlier pure policy work is not verified integration. Last audit 13:38; start unknown. | Design §16.5: bind attempt/stage/hard bound before GO, sample owned CPU/fork/exit without double counting, unavailable fallback, monotonic soft updates bounded by hard maximum, guardian enforcement independent of busy actor/service/SQL. Preserve sleep/resume limitations. |
| Frozen/dead owner and replacement-service recovery: SDK `crates/lean-toolchain/src/native_guardian/{ownership,record_lifetime_tests}.rs`, `native_guardian.rs`, `writer_lock.rs` | ABI owns current narrow collector preparation, **not completion of recovery design**. Last real Tiny failure has closure-before-permit observations; archived stale running record remains. Replacement-service adoption, trusted durable token discovery and frozen recoverer handling unverified. | Design §16.1–2: boot/post-exec identity, publication/arbitration before STOP/acquisition, complete SH recovery versus writer demand, two/frozen recoverers and supported lost-anchor cases. Narrow existing collector evidence is not takeover, writer reacquisition or original-result recovery. Needed before dependent service replacement promises. |
| One warm-generation/capacity owner: host `crates/lean-host-mcp/src/{broker,project}.rs`, `broker/control.rs` | ABI owner; current ordinary Entry and selected retirement paths passed, whole redesign unfinished. Last implementation progress 13:29 first-open source slice; current production turn admitted, exact start timestamp not retained. | Design §16.3: unify attempt/request/generation lifetime, sequential reuse, cancel and replacement, retain capacity until qualified closure; remove duplicate grants only after caller migration. Depends on recovery gates for service-loss cases. |
| Caller migration and removal of superseded authority: Proofs `scripts/stacks/src/stacks/{owned_writer,native_reader,artifact_reads}.py`, orchestration adapters; host/SDK shared reader/writer boundaries | No implementation owner assigned; coordinator owns cutover plan. Existing `docs/native-execution-cutover-plan.md` enumerates three Python custody families and launchers. Last source audit 13:38; start unknown. **Proofs Lean builds remain paused.** | Design §16.6: replace caller-owned helper lifetime/expiry/capacity with submit/get/cancel, preserve provenance and acceptance; migrate all supported launchers, remove duplicate controller/grant paths, qualify rollback and mixed-version refusal. Depends on production service, recovery and deadlines, not merely installer success. |
| Full integrated release and workflow acceptance | Root coordinator owns sequencing; overhaul coordinator maintains this ledger. Not started on final production candidate. | Design §16.7 after preceding implementation: exact-image mixed/fault workflows, platform and resource checks, then ordered SDK/host/semantic/dup publication and later separately authorized Proofs migration. No premature suites while implementation remains incomplete. |

Canonical heads independently read at 13:38: SDK `a24394e6b5c2330ccf3d81a67cea0bc94b103abc`; host `a608fabb22c125283e30c8dec8bcc904afe009a0`. Isolated archive/patch identifiers above are NOT these canonical heads. No final integrated release commit exists. Dependency assembly touches each repository's root `Cargo.toml`/`Cargo.lock` and affected crate manifests; semantic/dup owners are coordinated by `/root/lean_version`, whose exact current sessions are recorded in the cross-repository handoff rather than inferred here. Formatter release is separately complete, not evidence for lifecycle implementation.

Production installer turn `01a1eedf-8611-706b-ac51-a22d332091a8` was admitted after the 13:35 callback; exact start timestamp unavailable. Its last concrete change is the three-file patch reported 13:35. Dynamic authentication patch was reported 13:29; exact turn start unavailable. Future admissions should record UTC at dispatch, not infer elapsed active work from turn age.

Latest retained test evidence: installed ordinary Entry route passed; guarded Tiny still has no parity result. The guardian observation regression passed, then Tiny failed before calls because the worker command omitted the required full elaborator inventory. Corrected inventory worker `ae1b9d912a59e2ea4c720ad053f661145800a0fe3d0c9b83facb91bbe2f1e756` built at 13:31:34–39; no subsequent run. Failed worker/guardian closure was observed before permit return. Stale registry reconciliation and further fixture replay are deferred, evidence preserved. Expiry controller built at 13:33:52–13:34:03, lane released at 13:34:34 with 35.36 GiB free; its worker/native test are paused.

## Earlier checkpoint: 2026-11-18 13:09 UTC

- **Verified installed ordinary route:** private installer positive and crossed-record refusal passed; installed generation `generation-dyou7M` retained protected selection `ac716d964420158d69dd147e095f062305bd51c0268e4227ed9912448f1f2a26`. The actual installed ordinary query ran once at 13:02:31–13:02:58, exit zero. The cast helper ranked 2 of 10, score 208, with positive semantic evidence. Results were truncated with the lexical warning preserved. Independent review confirmed both actors' selected read domains, unchanged installed bytes, and later worker/guardian group absence. This does not establish pre-permit ordering or arbitrary-project/public release readiness. Evidence: `task-15/installer-actor-pair.3x8yXF/ordinary-installer-handoff/private-combined-entry-install/ordinary/`.
- **Tiny runtime next:** actual controller `0b7b51aa540c66c4743588c344ea8e734965ddd3fb9c3d811d8a4899d686f934` and worker `72f677b3fcd29d1d6f315b97133163f20b1e87019ed66262f035a411413e80d1` are independently selected. Actual epoch observation passed; the 40-entry runtime packet `5e78aec00ee0372fed1ec722a0cf2c3bf82504d15c955fb9ae13612872f5ac99` is now under independent execution review. No native Tiny session or strict 36-row comparison has run. ABI owner `6afca3dd-61c8-8191-beef-0e1235e410d2`, turn `01a1eec6-bbe9-74d7-a6a6-056537ff0f23`, is admitted for immediate follow-through after clearance.
- **Tiny attempt and correction, 13:14 UTC:** the reviewed staging passed, but the single runtime attempt at 13:10:57–13:11:03 exited 1 before admission with `historical owner closure uncertain`: zero new owners and zero captured frames. Later census found no relevant processes. Independent review confirmed the qualification observer incorrectly required a guardian group leader to be a session leader. Narrow patch `2aa02308b14cd4bf42f747ba40460d58134b62a52a940c20dbadcec4d453ff51` retains worker-session checks and requires exact guardian nonliveness plus unoccupied PID and absent group; unknown/reuse refuses. Source and two compile commands are reviewed; test and new matching controller/worker execution remain pending. The failed packet is preserved, not replayed.
- **Distinct fault gate:** installer owner `6afca1a9-4f1c-8191-a757-cb2c906df0dc`, turn `01a1eec6-aa2c-77e4-9de5-2101a4178639`, is admitted to isolated source implementation of post-authentication original-deadline expiry with an already queued request, zero dispatch and observed retirement before permit return. No fault build or execution is claimed; selected generations and Tiny inputs remain immutable.
- **Capacity and release:** Tiny worker compile ended at 13:03:57 with 36.47 GiB free; no compiler lane held here. Preserve the 25 GiB reserve. Final semantic Rust dependency alignment, general authorized project admission, fault/recovery gates, full release qualification and registry publication remain unfinished.

## Earlier checkpoint: 2026-11-18 12:44 UTC

- **Installer:** three filesystem tests passed. The real writer retry passed once at 12:39:49 after correcting fresh-registry creation to use `umask 077`; the earlier pre-custody refusal remains preserved. Complete synthetic actor-pair publication and protected rewritten-digest consumption by the broker reader passed; partial second-file collision preserved the previous selector and receipts. Registry was empty and three writer groups absent at 12:40:18. This is not yet actual installed ordinary serving or crash recovery.
- **Installer next owner:** `6afca1a9-4f1c-8191-a757-cb2c906df0dc`, turn `01a1eead-0e60-712a-a43f-c80b606d3b7c`, preparing actual reviewed Entry records plus installed protected selection for ordinary smoke. Source-only, no compiler lane held.
- **Tiny:** restored-path regression (one pass, 167 filtered) and selected-profile regression (one pass, four filtered) passed. Epoch observer compiled with 197 verified source hashes; binary `360e7fa32365e1bae10fcd596232c5e126574a806326513b8102405ff202d448`. Exact one-shot command `48b913892f1fd1ef6d9647486441c4afaf2be6dfff93e25be46c845782b00ebc` is selected with existing canonical private registry and `umask 077`; ABI owner turn `01a1eeae-4999-766d-a563-60a26aa3c9ec` is active. No observation result reported yet.
- **Tiny remaining:** actual epochs and file custody, final admitted serial metadata/features route, original response-frame capture, strict comparison against 36 native rows and retirement. Source review cleared ordered capture instrumentation for compilation, not native parity. No duplicate reference build needed.
- **Capacity:** observer lane released at 12:39:06 with 37.23 GiB free. No compiler lane currently held here; any next build requires fresh bounded allocation and 25 GiB reserve.

## Earlier checkpoint: 2026-11-18 12:10 UTC

- **Independently verified ordinary combined success:** one Entry `lean_lookup` proof search on SDK `b625433e` / host `5f8e7433` passed. The named cast helper ranked 2 of 10, score 208, with semantic fingerprint and conclusion-constant evidence. Proof state was contextual. Results were truncated and included a lexical warning, but no MCP error or semantic fallback.
- **Custody observations:** final snapshots contained seven main and eight companion read domains with selected epochs; controller and driver exited zero. Subsequent census showed both recorded workers, guardians and groups absent. This is not pre-permit ordering, raw Entry goal readback or arbitrary-project qualification.
- **Next active owner:** ABI session `6afca3dd-61c8-8191-beef-0e1235e410d2`, turn `01a1ee8f-fd43-735b-b8bb-b76ddd95bf76`, preparing the distinct guarded Tiny.Basic authority and ownership boundaries for ordinary fault tests and installer-generated general-project records. Existing artifacts should be reused; no new substantial build is allocated.
- **Capacity:** 12:08 UTC free disk 31.583 GiB, only 6.583 GiB above the floor. Old-target ownership inspection is requested; no deletion clearance or task-20 cleanup is authorized here.

## Earlier checkpoint: 2026-11-18 12:06 UTC

- **Ordinary route:** the first run failed before companion execution. Production position lowering used `Entry` while the selected record required `Default`. A red/green production-conversion test verified that mismatch. Separately, recovery obscured the primary exit with replacement refusal; focused successor regressions now preserve primary and secondary diagnostics. The original child stderr remains unknown.
- **Current selected source:** SDK `b625433ed519ba4db5aaff78ffef2d0b22c9bd8a`, host `5f8e74336bf93a7ad9dbfada9bc2d6c269dba480`. Controller `0ca2cbdf2b71c6e87eedf63f21752ed16b2e5cec8d6cd19caa9c7fd40d81217b` and worker `a1d59ddda3e2f000239cbc3d45538f85f12f4ab2b8b2c48573961a7a99a76b5e` built successfully. Worker closure checks cover nine images and 91 exports.
- **Current owner/action:** ABI owner `6afca3dd-61c8-8191-beef-0e1235e410d2`, turn `01a1ee8d-51ca-740f-9eb7-3b89adf056d7`, awaits independent selection of fresh runtime manifest `7250b59808344e9103ce174daeafaa0d649ab84599d4ef716cec6d95efbaa5d6`. No corrected Entry query has run. Driver captures changing actor read sets and failure census; response does not expose raw Entry goals.
- **Scoped recovery verified:** after observed group absence and birth-bound guardian non-liveness, actual existing collector retired the stale record; writer EX reacquired on the worker-output domain (one of seven), and fresh same-slot reservation reached `preparing_closed`. Original evidence archived. Other writer domains, new worker launch and pre-permit ordering remain unqualified.
- **Downstream:** semantic and dup successor static checks passed on `6aea09af`; standalone dup parity returned 36 complete equal rows with retired processes. Guarded SDK Tiny.Basic parity remains queued under separate authority. No registry release inferred.

## Earlier checkpoint: 2026-11-18 11:36 UTC

- **Owner and next action:** ABI/host owner `6afca3dd-61c8-8191-beef-0e1235e410d2`, active turn `01a1ee70-b5ee-76c8-9bc0-e1283576e6c8`, has been instructed to execute the independently cleared worker substitution regression, then the ordinary controller build. Admission and steering are verified; execution results are not yet reported.
- **Selected source:** SDK `9b8777dbc250edfa95e7cce7cf7e75119a3f0cc2` and host `944e50db979052b5333f6a89bec0fd5f47f4051c` bind actor role and exact record digest into the existing authenticated first open. Issuer regression, SDK formatting and strict Clippy passed. Independent review rehashed the compiled worker test and runtime closure plus ten controller inputs and cleared the exact commands. The focused comparison does not measure a live worker or retirement.
- **Remaining runtime gates:** build/select controller, then matching worker; ordinary combined proof-state/search, crossed-record predispatch refusal with actual retirement, cancellation/expiry and owner-loss recovery. The earlier portable companion success is retained below, not evidence for these new cases or arbitrary project provisioning.
- **Downstream:** semantic static qualification against SDK `6aea09af` passed at 11:21:35–47 UTC (focused/workspace tests, Clippy, docs and MSRV). Dup holds a separate bounded two-job lane for successor static checks and reviewed standalone native parity, not guarded SDK parity. No final successor runtime or registry publication is inferred.
- **Capacity:** at 11:28:39 UTC, 36.124 GiB free and 64% memory free; downstream two jobs and a separate ABI two-job target may coexist only with fresh preflight and the aggregate 25 GiB reserve. At 11:36 UTC filesystem free space was about 35.49 GiB. No additional build lane is allocated here.

## Earlier checkpoint: 2026-11-18 11:18 UTC

This checkpoint supersedes the historical session states below, which are retained as earlier evidence, not live status.

- **ABI and broker owner:** `6afca3dd-61c8-8191-beef-0e1235e410d2`. The 4.36 proof-state controller and worker built successfully. Worker build approval was requested at 10:04:25 UTC and completion observed at 10:05:02; Cargo reported 23.18 seconds. Worker SHA-256: `29f31dcd25a76663a5fe5ef5a82d166b655357ab9b526b840cfdbc08bb550198`.
- **Actual next operation:** independent review and coordinator rehashed all 13 inputs and cleared create-only staging followed by one proof-state query for packet `4eb0200e5b6806288a58d19677bcde2f19cc76444498cb7a5a6fe903980605e2`. Execution turn `01a1ee20-2c13-7369-b5fc-539d695dca42` ran once and failed with `operation outside fixed Init declaration inspection`, exit 1. Approval was requested at 10:08:08 UTC; startup was logged at 10:08:16 and shutdown at 10:08:28; completion was observed at 10:08:44. No typed goal or loaded receipt returned. Worker group absence and guardian non-liveness were observed while capacity remained zero, then capacity returned to one at 15.115 seconds. The failed generation is preserved. Diagnosis and a narrow exact-request regression are active in owner turn `01a1ee22-0be4-71e4-be37-ad5cf16a91d7`, with independent review. The generic error alone does not identify which policy check refused. Independent source review and the owner both found a deterministic producer defect: a missing NUL separator after the file label produced a different cache key. The correction changes only the selected cache policy (grant `642a69066767e38f6dbadbda34f2157bcc14fb66b66dee65572e640c53154bf2`). The actual SDK key/lowering regression passed once (one test, 38 filtered). Corrected controller `b3e268347382c136eeb1cf2b07dbfd5e7211db456613f277804c7f1752b0753f` and worker `ece0f4da3ab9ecb1ebc64692745d00895731a49b37e19f52ab326fbc5ad5a310` built successfully (Cargo 8.38 and 4.24 seconds). Fresh packet `f327a50e20dfa663c59a662da05a657c8ae2632bffd35f7f1e3f095fc73e13bc` has 13 verified review inputs. Its first staging attempt refused the stale grant digest hardcoded in the staging script before the first write; no worker or query ran. The failed stager is retained. After independently reviewed pin correction, staging and the first corrected query passed, exit 0 (approval requested 10:25:32 UTC; completion observed 10:26:05). Actual `miniRatDenominatorStep` proof state at `skip` returned the expected namespace and `q : MiniRat` cast/multiplication goal, empty diagnostics, no truncation and cache miss. Loaded receipt matched Lean 4.36/header and seven ordered epochs. Worker group 9879 was absent and authenticated guardian 9880 non-live while capacity was zero; capacity returned to one at 15.096 seconds. No retry or companion run occurred. Independent receipt review passed; no warm hit or general project support is inferred.
- **Verified earlier runtime:** the separate main declaration packet `b244b4274f7bb12bdfd7b15a0f8b51aa7602101b5f45083e9e18cc50ee7d20d6` returned the expected definition on 4.36 with independently reviewed retirement evidence. This does not qualify cached proof-state or semantic search.
- **Semantic companion:** private, unchanged-guard local provisioning passed. The corrected own-RPATH artifact has manifest `8fb7be220eef9607f27d21f2e45c3aa49818059aedf4cdebf5345622493924a4` and dylib `1a5630f4f8779f0b97a24c8b62ef1f16df547c21efb055e8bad8a6540cff3973`. Actual capability loading/search remains pending its own protected authority. Companion selection `b98ed41561fed1890704c14a33231b5bc1363e4b5623228b5dcd635dff1c8c43` binds the measured proof-state payload, 93 rehashed files and exactly two exports; independent input review passed. Current worker still refuses capability mode. Owner turn `01a1ee37-b39b-757a-aabe-7f595f3e51f5` implements the narrow existing-SDK capability adapter and rejection tests. Manifest/image/ordered search paths must be verified under retained guardian custody before initializer/symbol calls. The adapter helper and actual restored-path transaction regressions passed. Portable artifact selection `8f8e239e524901dbdd47f3449395b4608bdb8be24d2550a332505462f5aec8fb` supersedes the private artifact for final qualification; it binds 93 files and unchanged command payloads. Portable source/provision/cold-warm gates passed under the downstream owner. A controller compile failed on an incorrect observation-method receiver; a narrow correction captures the opened companion session identity, with the failed log retained and revised host `446062b51992c8f0221386972814ff90ef4d2b81` under compile review. Corrected controller `29648d187c8dd0ccbf4262eb6d96fd6d929d6a334817074a78943ae7cee82c69` and worker `c2cd756f8af55ab1ce23abf02078105e6e93eba22caa09dfd0cf07cfbd34403f` built and were independently selected. Packet `01d3692fc13cc494e5ee225c6fe3b2b4b5ba42f8e8a42c5a5d8dc58a37b34be4` passed its FIRST actual guarded portable companion invocation, exit 0 (11:00:13 approval request, 11:00:38 completion observed). It returned one goal row, nine declaration rows, no diagnostics, and exact helper rank 2/score 142 with real semantic match evidence. Independent review matched companion ID across result, loaded 4.36 receipt and retirement; eight epochs included writers 5466/12558. Worker 15890 group absent and guardian 15891 non-live at 15.848370 seconds while capacity was zero; capacity returned at 15.859630. Zero retries or fallback. This qualifies the selected fixture, not arbitrary projects. Owner turn `01a1ee51-c3b8-76d4-b30f-148b3c386183` now prepares coherent release ancestry and general authorized project integration; do not repeat the fixture. Machine-specific RPATH is not a portable release solution. Earlier metadata-only fingerprint changes remain unexplained; the guard was not weakened, and a metadata-only rejection regression passed.
- **Downstream coordination:** `lean_version` owns semantic and dup lanes. Semantic final nonruntime tests (66), Clippy, docs and MSRV passed against the private SDK snapshot; final nondiagnostic cold/warm cache test passed. Dup final nonruntime tests (9), Clippy, docs and MSRV passed; seven focused standard-ELAN_HOME tests plus Lean-free installer probes passed. Neither establishes native installer/loaded-companion qualification or registry readiness.
- **Capacity:** approximately 41 GiB free at this checkpoint; retain the 25 GiB reserve. No duplicate artifact build is needed for the selected semantic companion. Proofs Lean/Mathlib builds and cache deletion remain excluded.
- **Remaining release gates:** general authorized project support; downstream final native parity and packaged public installer; integrated cancellation/failure/recovery and custody checks; canonical integration, unpatched dependency/package gates and qualified publication. No production replacement-service or deployment claim follows from the private packets.

Exact current receipts and manifests are under `task-20/broker-grants.R2fHSM/audit-five-v2/proof-state-proposal/` in the retained local qualification workspace. The artifact selection is immutable by the hashes above; source review, execution and publication remain distinct.

### Release handoff and next actions

- Immutable tested SDK archive: commit `356b3d8d0af440870f4a34761ea22b0fcd4d4e7e`, SHA-256 `3d1a5a50c5d03117e93aa7772906c773944e3b4e787652af9543a958589209b5`, retained at `audit-five-v2/release-integration/sdk-356b3d8d.tar.gz`. Intended version is 0.9.10. Independently cleared for downstream source tests; not registry-ready. The ABI owner retains SDK/host publication ownership; `lean_version` coordinates semantic/dup.
- Canonical SDK `a24394e` is an ancestor, but cumulative integration is 51 commits/180 files and still requires final gates. The tested source failed rustfmt. Successor `6aea09affab9c3a94296c27c2546fdb2a1d28ab4` passes formatting, strict all-target Clippy (`sdk-clippy-10.log`), warning-denied rustdoc and Rust 1.94 all-target checks. Earlier `sdk-clippy-2.log` genuinely failed and is preserved, not current success evidence. Successor changes include an explicit unwind-safety verifier bound and are under independent review, not merely formatting. Final API/package/platform and unscoped-worker checks remain required.
- A separate paired-actor-record patch compiled and its actual focused Rust test passed (one test, 601 filtered). Independent paired-record source review passed. Host `4a3bbf20` now takes mandatory independently selected main and companion records at ordinary Darwin serve startup; CLI and pairing regressions passed. First-open currently does not bind the actor record itself, so owner turn `01a1ee61-41b5-7660-bf18-45592d55e9c6` must add that commitment through the existing guardian exchange and test record substitution with the request unchanged. Combined ordinary search and failure/recovery remain unqualified; fixture pins must not become ordinary policy.
- Owner turn `01a1ee57-6a15-75b4-8159-d9333ae1632c` continues SDK gates and ordinary delivery implementation. Downstream owners received the immutable archive and can run source gates concurrently. Required runtime negatives include crossed/stale records, changed manifest/export/path before load, queued cancellation/absolute expiry, owner loss during transfer and resource retention until closure. Keep prior positive receipts rather than rerunning unchanged fixtures.

## Historical checkpoint and ownership records

Snapshot: 2026-11-17, 20:41 UTC. Maintainer: tooling coordinator. This is the current gate ledger, not a second design.
Requirements remain in [lifecycle §16](native-execution-lifecycle-design.md#16-remaining-implementation-and-acceptance-gates).
Update this file after reviewed results, ownership changes or build-lane transfers; do not mark a gate complete from a
session ending. Distinguish source review, executed tests, canonical integration and deployment.

**No replacement production service or deployment is qualified.** Overhaul work proceeds in parallel with Lean 4.36;
only shared source ownership, immutable SDK compatibility and substantial builds impose dependencies. Proof Lean work
remains paused. Mathlib and its caches are outside these tasks.

## Historical gates (superseded; not current status)

| Gate | State and evidence | Owner; next action or blocker |
| --- | --- | --- |
| Frozen scope and cleanup | **Unfinished.** Earlier parent-alive CONT evidence is narrower than service recovery. Current no-extra-lock integration has zero native executions. Private startup retention passed source review. A public pending-start API patch now preserves actual child/control before READY; two production and three fixture migrations plus CLI/template changes are now source-only patches under independent review, including a new worker retirement _exit boundary. Revised reporting state distinguishes local-write completion from unknown delivery across Drop/unwind; dedicated-worker capability and thread confinement are source-only under review. Seven harmless subprocess scenarios await compilation/execution; review found arbitrary emitter claims and assertion-panic false-pass risk. Revised source uses an SDK-owned bounded FIFO write and separate exact test-completion packet with deliberate assertion-failure negative control. Independent re-review passed; three exact harmless tests passed, including nine status modes and rejected false-assertion control. Disposable SDK/MCP binaries linked and generated API baselines matched; local dependency overrides and two initial offline failures are preserved. Independent evidence review passed. Exact qualified bytes are committed only on isolated SDK series 864686c → 5baf9ce → 9d13fe6 and host b8fd321; combined ABI composition needs new checks. Real freeze remains prohibited. The child.rs hunk is held for ABI sequencing. Real freeze tests are withheld: W may park indefinitely and the runner has no independent survivor cleanup. Four harmless runner checks passed (reservation, exit 103, interruption, 30-second timeout); this is admission bookkeeping, not guardian recovery. | Guardian session below: finish/review coherent caller migration, bounded terminal reporting and harmless API fault tests; prevent dropped cleanup errors/owners and coordinate worker-entry ownership with ABI. Real STOP cases need independently controlled containment; no eligible disposable guest is established. Then qualify normal CONT, omitted CONT and early loan loss with the actual SDK loan as sole protector. No observed surviving guardian is claimed. |
| Registry recovery and admission | **Partial canonical fixes.** SDK `120767d` separates record retention from compute occupancy; `4ae59cd` drops ungranted writer candidates before owner waits. | Coordinator/guardian: replacement-service token/boot admission, publication, recoverer arbitration and own-crash behavior remain unresolved. Current absence and unknown historical result remain separate. |
| Warm generations and capacity | **Isolated candidate tested, not integrated.** Canonical dormant pin is SDK `a24394e`. Paired candidate has 25 dispatch and 70 supervisor passes (14 overlap), eight compiled mutation controls, Clippy and shutdown harness. Independent review verified seven checkpoint hashes and logs. | Warm session: unwind sealing guard and caught-panic regressions are now source-only additions; independent source review passed; revised 26 positives passed before/after all nine compiled mutation controls and strict Clippy. Seven hashes matched throughout; real native custody remains unqualified. Then review breaking refusal API with host retirement, preserve first error and capacity until actual closure. Real custody, durable handoff, closure/rearm and production activation remain open. |
| ABI safety prerequisite | **Incomplete isolated migration.** SDK checkpoint `352c4dadbb2e9a9d5a3bde5b88e925636d8de7c6`; preceding compile-fail checks passed, native valid-object tests were compile-only, host checking failed on unfinished adapters. | ABI session: unified import transaction has five standalone passes; installed-header allocator probe accepts three old toolchains and rejects non-mimalloc fixtures. Host integration and runtime/header pairing remain unverified. Bridge source now covers 23 of 51 rows; new bindings and host import/pool/query migration remain uncompiled. Independent source review of both task-20 patches found no lock/signature defect; eager shim exports and new target C-tool requirements need compatibility qualification. Third-patch review found unbounded attestation-key diagnostic history and independently pairable prepared metadata/loaded image. Fourth hashed patch bounds diagnostic shapes to 256 and binds custody/image loading; standalone history stress passed, source fixes are under review and native mismatch rejection remains untested. Review also found three explicit Lean Unit arguments incorrectly called as nullary; correction and generated-C signature checks block affected native calls. DOCS_RS=1 cumulative crate check failed with 17 host migration errors; fifth hashed patch migrates these plus worker loader/four caches and corrects three explicit Unit arities. Source review and repeat compile are pending; next-lane-check.sh is syntax-checked for documentation/configured worker-child checks and five regenerated C signatures. No compiler/native call ran in that preparation. Fifth review confirmed Unit fix but found worker snapshot/ordered-path admission and native module-cache environment identity gaps. Sixth hashed patch repairs snapshot/path admission and native cache scope identity; independent source review passed, but equal-content/distinct-attestation cache oracle and end-to-end bootstrap negatives remain required. Configured Lean 4.35 worker/host targets now compile, and regenerated C confirms the five affected signatures; native regressions remain unrun. Generated signatures also match on 4.33-rc1 and 4.36; seventh revision passes all 14 compile-fail fixtures and positive control. Worker libs/bins compile on 4.33-rc1/4.35 and 4.35 test binary links. Configured 4.36 rejects its unregistered header; no full 4.36 SDK support yet. Not a requirement to finish every 4.36 project first. |
| Store, protocol and authority | **Private reference tests only.** Host `cf9bc4e`, `6ceb212`, `c8126f5`, `3a310c9`, `ea996dd`, `07c9f6d` record bounded store, SQL-failure, caller-retention and same-owner runtime work. | Coordinator: production placement, authenticated bootstrap, corruption/restore, resource registry and dispatch remain unwired. Do not add storage features to defer custody decisions. |
| Live activity and deadlines | **Partly specified, not qualified.** Host `bd9783d` and regression correction `c6357bc` preserve admission deadline; this is not guardian-enforced request lifetime. | Coordinator: implement and qualify deterministic activity accounting, unavailable fallback and independent hard bounds after ownership contract is settled. |
| Abstract model | **Bounded validation complete.** Host `a309cc600b96040e19b487cf91c831b95867f05c`: five modules parsed; recorded 155 required expected outcomes (59 positive, 86 negative/witness, 10 liveness negatives). Larger combined run remains incomplete. Duplicate Python explorers removed. | Coordinator: refine authoritative TLA+ for the selected recovery/warm protocol as it settles. Earlier model is not proof of current implementation, OS custody or restart recovery. See [model scope](../specs/native-execution/README.md). |
| Cutover and exact-image acceptance | **Planned, not executed.** [Cutover inventory](native-execution-cutover-plan.md), introduced at host `e64c351`, covers legacy launchers, ownership deletion, fencing and rollback. | Coordinator: after applicable implementation gates, run exact-image mixed/fault workflows; obtain deployment authorization separately. No replacement deployment or proof acceptance claimed. |

Commit IDs refer to the named repository and bound the corresponding source checkpoint, not every later working-tree
edit. Uncommitted candidate results apply only to the reviewed checkpoint hashes; new edits require new qualification.
The coordinator verified session outcomes and the warm reviewer inspected hashes/logs, but did not independently rerun
all historical campaigns.

## Historical ownership and evidence locations (not live status)

All sessions below were created with an explicit GPT-6.2 model selection; runtime model identity is not independently
exposed by the status interface. Released older sessions must not resume overlapping work.

| Lane | Durable session | Local handoff/evidence |
| --- | --- | --- |
| Guardian cleanup (Astra) | `6afca2af-3368-8191-9816-a92e53fcd0c4` | `/Users/jcreinhold/Documents/Codex/2026-11-17/task/guardian-resume-integration-handoff.md`; `/private/tmp/lean-sdk-resume-integration-20261117/` |
| ABI migration (Astra) | `6afca3dd-61c8-8191-beef-0e1235e410d2` | `/Users/jcreinhold/Documents/Codex/2026-11-17/task-8/ABI-SAFETY-HANDOFF.md`; isolated `task-8/lean-rs` |
| Warm paired contract (Astra) | `6afca23e-b9c8-8191-a3ef-228507da80ec` | `/private/tmp/lean-warm-integration-20261117/SESSION-HANDOFF.md`, `paired-candidate-checkpoint.json`, `paired-restored.log`, `paired-supervisor.log`, mutation logs |
| Formatter 4.36 (Sol) | `6afca213-d120-8191-95ff-74a0d82b80d3` | `/Users/jcreinhold/Documents/Codex/2026-11-17/task-10/HANDOFF.json` |

Temporary paths are working evidence, not permanent publication. Preserve source hashes and reports in durable handoffs
before removing a scratch tree. Archived model evidence and replay instructions are documented in the specifications.

## Build lane and immediate sequence

- One substantial build lane, at most two jobs. Warm released it at 18:54:08 UTC after sequential qualification.
- Formatter released the lane at 19:27:20 UTC with no owned processes. Isolated `ee1c6b6` has a 39/40 full run
  followed by passing Modes/downstream after the sole whitespace correction. Both worktrees are clean; canonical fmt
  remains `81899c2`. A cold Lake plugin race was preserved; Linux remains unqualified.
- Guardian released the lane after harmless qualification. ABI released the two-job lane at 20:26:30 after ninth-checkpoint all-target checks on 4.33-rc1 and 4.35; source-only fixture-policy preparation is active. Seventh source patch admits approved runtime/manifest before loading; review and native cache/bootstrap tests remain pending. Eighth patch migrates stale callers; all worker-child targets compile on 4.33-rc1/4.35. Ninth patch uses opened regular/nonblocking manifest checks and validates allowlist before load; independent Unix source review passed, zero-initializer regressions unrun. Local one-shot qualification can use pinned images and cooperating custody without new mounts/accounts; hostile-writer guarantees are excluded. Existing host/dup owner maps concrete stock policy/custody inputs; combined guardian validation remains open; real freeze/native ABI gates are unchanged. Guardian native qualification remains blocked on safe containment regardless
  of lane availability; source repair continues. A lane grant is not deployment or proof/Mathlib authorization.
- Semantic 4.36 Lean build/tests passed on an isolated archive of `333c219`; its Rust/runtime qualification still needs
  an immutable SDK. SDK isolated `750846a1c27783fbc7b71c652fe596aa36635b52` removes the two missing string exports coherently; all 91 retained exports pass bounded audits across four inspected Apple Silicon libraries, including 4.36. Independent ABI review confirmed consistency; integration requires audit `e9baea03` followed by removal `750846a1`, not removal alone. Cargo/Lake qualification and runtime/header pairing remain open. These are
  compatibility inputs to the ABI family, not reasons to halt independent lifecycle work.

Coordinator checks completion callbacks first and checks active lanes about every ten minutes. Twenty minutes without
new evidence triggers a specific progress/current-command query, not an automatic kill or duplicate session.

## Timing and last evidence

As observed on 2026-11-17 at 20:25 UTC. Session attachment is metadata, **not execution start**. The supported
thread reader reports turn/status but no dependable turn-start time. Times below are explicit lane records, coordinator
observation windows, or UTC file modification times (labelled mtime). Neither an open session nor elapsed wall time
proves continuous work. Do not estimate active compute time from these fields.

| Track | Session attached (UTC, where checked) | Current step and last evidence (UTC) |
| --- | --- | --- |
| ABI | 18:58:39 | Current turn `01a1eb2d-b0b5-731a-b7fb-514d4c909a38` admitted around 20:23; exact start unknown. Eighth 4.35 all-target log mtime 20:21:51, final incremental Cargo duration 1.62 s. Prior completion observed 20:22; ninth ingress-fix checks passed at 20:26:26–28 (4.33-rc1) and 20:26:28–30 (4.35); two earlier exit-101 attempts preserved. Lane released 20:26:30; source-only fixture-policy preparation. |
| Guardian | 18:53:37 | Qualification finished by 19:59; immutable isolated commits reported by 20:03. No active build; waiting coordinated composition. |
| Warm | 18:51:44 | Revised campaign explicitly claimed lane 19:28:08 and released 19:28:22 (14 s lane occupancy, not summed CPU time). 26 positives before/after, nine mutation controls. Waiting joint integration. |
| Formatter 4.36 | 18:51:03 | Full-suite lane status recorded 19:08; final release 19:27:20. Final Modes 105 s, downstream 24 s. No current build; Linux remains unqualified. |
| SDK 4.36 audit | 18:50:49 | Isolated removal `750846a1` completed before 19:07 coordinator check. Exact task start/end unknown; waits coherent ABI composition. |
| Semantic 4.36 | Not rechecked | Exact lane release 18:51:42; Lean build/tests passed. No current build; Rust qualification waits SDK. |
| Host/dup | Not rechecked | Policy-source mapping turn `01a1eb2d-c0ed-7545-adbc-85358bf63e09` admitted around 20:23 and completed by 20:25; read-only, no lane. It identifies application worker entry plus controlled fixed-fixture approval records; production provisioning is not implemented. Prior qualification plan is not runtime qualification. |
| TLA/model | Historical session | Completed bounded evidence in the model gate above. No active model run; no inference of implementation proof. |

For subsequent commands, owners record UTC start/end, exact command and snapshot, result, and lane release in their
existing lane/handoff file. The coordinator records the last verified checkpoint and next dependency here. A missing
time stays unknown. Filesystem times must be rendered with `TZ=UTC`; an mtime is only the last recorded file write.

At 20:31 the ABI owner reported a draft fixed-fixture policy/sealer/launcher, not a compiled application provider.
Review must settle pre-exec runtime pairing, retained image lifetime and practical custody without assuming new mounts
or provisioning authority. Only isolated host-entry composition is assigned; no canonical worker entry changes.

At 20:40:44 the current ABI turn had about nine minutes of observed wall time since admission around 20:32.
Latest explicit source checkpoint was 20:31:19; new isolated sdk-composition files were present within the preceding
ten minutes. No active compile was recorded. This is progress evidence, not continuous-compute or completion timing.

## Cross-track assignments

Updated 2026-11-17 at 20:58 UTC. This table includes waiting work, not just running sessions. Assignment times are
coordinator observation windows unless an exact admission time was returned. No completion range is established for
unfinished integration; use the next evidence checkpoint, not an invented ETA. Callback review plus approximately
15-minute coordination checks remain in place; twenty minutes without new evidence triggers investigation, not a kill.

| Work item / owner | Assigned UTC | Deliverable / state | Last evidence / blocker | Next checkpoint |
| --- | --- | --- | --- | --- |
| ABI and SDK composition / `6afca3dd-61c8-8191-beef-0e1235e410d2` | Current turn around 20:32 | Active isolated policy/harness and integration | SDK composition `16b625ac` combines guardian, ABI and ordered raw audit/removal. Host fixture entry compiled 20:41:01–11; SDK all-targets 20:41:37–42; lane released. Runtime pairing/native tests and production policy remain open. | Reviewed no-load rejection controls, matched one-shot fixture; no unrestricted authority fallback. |
| Guardian / `6afca2af-3368-8191-9816-a92e53fcd0c4` | Latest commit assignment around 20:01 | Waiting; immutable qualified patch series delivered | SDK `9d13fe6`, host `b8fd321`; harmless evidence reviewed. Real STOP/loan/group recovery unqualified. | Combined ABI composition checks, then separately gated real recovery. |
| Warm / `6afca23e-b9c8-8191-a3ef-228507da80ec` | Last qualification 19:28:08; resumed around 20:45 | Active isolated candidate commit/private host adapter implementation | Released 19:28:22; 26 tests and nine mutation controls. Breaking API/host retirement not integrated. | Compose reviewed API and host retirement without changing prior-stage outcome semantics. |
| Formatter 4.36 / `6afca213-d120-8191-95ff-74a0d82b80d3` | Resumed around 20:42 | Locally integrated after independent source/evidence review | Canonical main fast-forwarded to `ee1c6b6`, clean/ahead three; untracked candidate files preserved. Mac evidence complete within stated scope; Linux unqualified. | Qualified release gates and publication remain separate; no push/tag/deploy performed. |
| Formatter independent review / coordinator's `warm_paired_review` | Around 20:43 | Complete: approved local integration, no source blocker | Same candidate; no build lane or edits. | No active formatter review; retain full-final-head/Linux limitations. |
| Semantic 4.36 / `6afca18f-2580-8191-a605-09e8794b90cc` | Resumed around 20:42 | Locally integrated after independent audit; no runtime-release claim | `333c219` Lean build/tests passed; canonical main now `c30d8b3` including reviewed two-line MSRV CI fix, clean/ahead four, no push. Rust/loaded worker waits coherent SDK. | Rust/loaded-worker qualification on coherent SDK; CI MSRV consistency and downstream pin preparation. |
| SDK 4.36 audit / `6afca208-0fdc-8191-a3bf-690b80d430ff` | Earlier time not reconstructed | Waiting; audit/removal delivered | `e9baea03` then `750846a1` now composed by ABI owner. 4.36 header still not approved in configured support. | Paired runtime/header/allocator qualification before support activation. |
| Host/dup / `6afca1a9-4f1c-8191-a757-cb2c906df0dc` | Policy mapping around 20:23 | Waiting; read-only mapping complete by 20:25 | Canonical host/dup upgrade audit prepared; runtime qualification waits SDK/semantic. | Match immutable family and run isolated worker/install/mixed-version checks. |
| TLA/model / prior model owner | Historical, unknown exact start | Waiting; bounded model work complete | Earlier model, not current production-recovery proof. | Refine when selected protocol stabilizes; no duplicate Python explorer. |
| IUT IV Proposition 1.1 / `6afcbc56-5da0-8191-bcc7-90c5e9b7e0ff` | Around 20:44, admission confirmed by IUT coordinator | Completed non-Lean source/construction plan; commits `97790c2f1d` and `a7b9c8ba83` reported by IUT coordinator | New session; primary-source evidence, not checked Lean. No builds or blueprint edits. | Reviewed construction plan and scoped gated documentation commit. |
| IUT Riemann existence / `6afcbc63-4e6c-8191-ae2c-9f09266f40e1` | Around 20:44, admission confirmed by IUT coordinator | Completed non-Lean source resolution; commit `bc2c4ca264`, clean Proofs tree reported by IUT coordinator | New session; no builds, .lean edits, cache/install or blueprint changes. | Resolve actual source blockers and commit only supported documentation. |

The single substantial build lane serializes CPU/memory-heavy campaigns, not independent source work or reviews.
Formatter and semantic review now proceed beside ABI. Local integration, publication/tagging, deployment and Proofs
upgrade are separate outcomes. Proofs requires the matching formatter/SDK family and Mathlib version; its live pins and
cache remain untouched.

At about 20:49, formatter and semantic source upgrades are integrated locally, not published. Warm SDK candidate is
`fbf0e62`; private host adapter source is under joint review before tests. Host/dup owner is preparing independent
Lean-only pins. New IUT read-only native Mac audit `01a1eb44-d62f-7830-b8f7-034e4ca1f26c` compares Mathlib 4.36
release/signatures/consumers; no builds, fetch, cache or source changes. The owner subsequently permits whichever session type is best for management, preferring the Mac executor for repository work. Existing owners remain assigned; no duplicate sessions are needed.

### Current checkpoint: 20:58 UTC

- ABI turn `01a1eb35-ab0c-724b-8e4b-5e03f7a13597` completed. Composed SDK `16b625ac` contains ten ABI patches, guardian changes and raw-symbol audit/removal. Host `b8fd321` and SDK all-target checks passed on 4.35. Two fresh-process rejection controls passed using extracted production functions/test doubles, not a Lean native regression. Lane released at 20:48:33. New source-only provisioning turn `01a1eb4b-f8a4-77c4-9bfe-1d28fa577748` is admitted; runtime/helper identity, load-command resolution and pre-exec custody remain required.
- Warm turn `01a1eb4b-df9e-723b-9bf4-c52d4abd751f` is admitted and owns the single two-job test lane. Independent review found missing invoked regression coverage and seal-error/primary-error preservation gaps in the private adapter. The assignment includes frame counts, one-use consumption, validation ordering, failed retirement and slot retention. Canonical activation remains disabled; admission is not evidence tests have run.
- Host/dup completed an applicable source-pin patch against `a1189fb`: six Lean toolchains and CLI default to 4.36, both Lake semantic pins to `c30d8b3`. Static checks passed; no compile or canonical change. Lean-only checks wait for the lane; Rust/package checks wait for the coherent SDK family.
- Native Mathlib audit completed at 20:53:33, after 4m54s, per IUT coordinator. Four source-inspected candidates, no compiled theorem claim. Proofs remains clean at `bc2c4ca264`; no active IUT task.
- Mac briefly disconnected and was confirmed connected and reattached before this update. No uncertain command was rerun. No release or deployment occurred.

### Qualification handoff: 21:02 UTC

Warm released the lane at 21:01:50 after 27 receipt tests (four new regressions) and strict host Clippy. The revised
private adapter patch is `07c1fb273ad894967c89aec0efd5f9dd51fc0fd0f13419053939be88fe9ef17b`; independent joint review
is requested. Tests cover body/seal/shutdown outcome separation, framed 0/1/2/2 counts, queued exclusion and retained
companion capacity. They do not qualify authenticated GO or durable native custody; canonical activation is disabled.

ABI provisioning resolved the reference worker's nine-image closure, but that binary predates the fixture policy and
cannot authorize a positive run. The source-only supervisor/driver now has independent review assigned. ABI receives
the released two-job lane for compile-only supervisor and actual fixture-feature binary/manifests. Native execution
still requires review of these exact identities, load commands and allocator provenance. No positive ABI test has run.

### Build/review handoff: 21:11 UTC

ABI compile-only completed at 21:10:43; worker `95322540…`, revised supervisor `51acdf3f…`, outer record `6e1c0b70…`
are recorded in `task-20/fixture-policy/candidate-evidence.json`. Neither executable ran. The supervisor now owns the
actual worker; independent exact-source/artifact review remains required before one native positive. The lane was
released and assigned to host/dup turn `01a1eb59-da7f-7693-a990-d6db443f936e` for isolated Lean-only 4.36 qualification,
not unqualified SDK/Rust execution. ABI review proceeds independently and waits for the lane before any approved run.
Warm's revised private checkpoint passed independent review: its retirement failure is injected, and successful framed
cases use the post-arm seam. It does not establish authenticated GO, production shutdown precedence or native custody.

### Native run and strict-build repairs: 21:15 UTC

Dup released the lane at 21:13:21. Ordinary four-fixture builds and duplicate-key JSONL recovery passed on 4.36;
strict builds exposed a deprecated import and two unused binders. The reviewed minimal repair replaces the import in
both worker copies and renames only unused fixture binders; strict rerun is pending. ABI turn
`01a1eb5c-48c3-7349-b34b-a83face71dd6` owns the lane for exactly one independently approved native fixture run.
All 91 artifact hashes and corrected actual-child ownership were independently reviewed. Execution result remains
pending; no production, group-closure, theorem or 4.36 ABI claim follows from this bounded fixture.

### First native fixture result: 21:16 UTC

The approved supervisor ran once at 21:14:57–21:15:00 and returned zero, independently read from `one-positive.exit`.
Its successful path requires 6×7=42, termination acknowledgement, directly owned worker exit and all five writer
reacquisitions. No retry/forced termination occurred; post-run helper/runtime hashes matched. Explicit lane release
was 21:15:16. This is cooperative pinned 4.35 fixture evidence, not imports/theorems, process-group closure, stock
providers, 4.36 or production qualification. Dup strict rerun now owns the lane in turn
`01a1eb5d-d62f-7224-ac96-a8484f65f6fb`; ABI prepares the next finite gate plan source-only.

### Integration and next ABI gate: 21:20 UTC

Independent review cleared the exact lean-dup source-pin and warning patches after strict Darwin arm64 tests.
Canonical integration is assigned to its existing owner, turn `01a1eb61-c37b-77d2-9700-7f5b9498016c`; it is not yet
reported complete. Rust fallback toolchain changes too; packaged installer and remote/Rust/SDK qualification stay open.
ABI's `task-20/NEXT-ABI-GATES.md` specifies one scoped 4.35 import/theorem check, not another multiplication run.
The owner is assigned compile-only guarded-entry/prepared-shim wiring and installed olean/elaborator provenance on
the released two-job lane. Exact artifacts require independent review before that native run. 4.36 needs its own
paired configured build/signature/image evidence; production provisioning remains a separate explicit decision.

Lean-dup local integration completed at `19fc765ae4b4291b895ec059f8de4f9e5df2f8cd`; coordinator verified clean canonical
main and the 15-file commit at 21:21 UTC. The exact patches had passed strict Lean/direct-worker/four-fixture/JSONL
qualification. Rust/SDK/package and remote dependency reproducibility remain pending. No push or publication occurred.

### Scoped import/theorem review: 21:37 UTC

ABI compile-only scoped 4.35 harness completed and released the lane at 21:36:23. No native import/theorem ran.
`task-20/scoped-435/REVIEW.md` and exact artifact hashes are assigned to independent review, including the added
guardian/group closure checks and six writer probes. Reported official-prefix comparison covers 15,291 selected files
and 105 fixture files; this is not itself executed ABI evidence. 4.36 metadata remains unapplied. Formatter's existing
owner is preparing a finite release-gate plan source-only; no repeated integration or publication is authorized by that
assignment. The lane is free pending review, not occupied by these source checks.

### Final formatter coverage and admission diagnosis: 22:12 UTC

Formatter finished local final-head coverage at 22:02:38 after starting 21:41:09: build, lint, 52 unit tests and all
40 suites passed across an interrupted invocation and a four-suite continuation. Temporary fixture edits were restored;
canonical main is clean. Publication awaits the existing approval question, not another request.

The scoped ABI run failed before startup with unlabelled EPERM at 21:39:39; it supplies no import/theorem evidence.
The admission-only probe compiled at 22:03:55 and released the lane at 22:04:09. Independent review verified binary
`c4749564a415fcfcadf5fd37d6c5a2519adfedfae269514a6549bd8ad8dfc040`, helper, SDK and six domains. One diagnostic
turn `01a1eb90-ca94-70e7-b051-637cad9ada2a` was admitted about 22:11 UTC under original restrictions, without worker,
guardian or privilege changes. Outcome pending. Any labelled error identifies this invocation, not the original cause.

### Admission permission boundary: 22:14 UTC

The single diagnostic ran 22:11:52–53, exited one and specifically reported `UnixListener::bind` EPERM for reader
`fd.sock` after creating its private temporary directory. No worker/guardian started; original evidence and demand-lock
identity were preserved. This identifies the diagnostic operation, not the original unlabelled qualifier cause or an
exact policy rule. Native work is blocked pending an explicit execution-policy decision for one pinned admission-only
listener/helper handoff. No alternate path, escalation or custody removal was attempted. The owner proceeds with
source-only 4.36 metadata/signature reconciliation; configured support and release activation remain unqualified.

### Scoped 4.35 success and isolated 4.36 build: 22:21 UTC

Supported per-command approval permitted the unchanged admission probe at 22:15:08–09: six descriptors acquired,
exit zero, original failed evidence preserved. Separately approved scoped import/kernel execution ran once at
22:18:06–22:19:46 and returned zero; lane released 22:20:00. It checks the fixed private import/theorem, bounded
Checked/type assertions, direct-worker exit, inherited-group absence, guardian non-liveness and six writer
reacquisitions. Exact proposition/epoch/binding equality, detached descendants and guardian reaping remain unasserted.

Isolated 4.36 paired-build turn `01a1eb99-5553-77e3-b448-8c7ce0aa52a8` now owns the two-job lane. Independent review
approved the two-file registration patch only for that isolated build, not canonical activation. Required evidence:
fresh allocator positive/non-mimalloc negative controls, rebuilt host/interop/fixture images, all 53 signatures including
multiplication, ownership checks and actual runtime closure. No 4.36 native execution is assigned before artifact review.

### 4.36 paired compile and migration blocker: 22:27 UTC

The isolated 4.36 fixture compiled; allocator positive and same-header non-mimalloc rejection controls behaved as
expected. Owner records all 53 fresh signatures/exports, three initializers and nine-image closure. No native executable
ran. Independent review of the immutable `task-20/paired-436` packet is assigned. Broader configured SDK all-target
checks fail on 29 core/host tests, examples and benchmarks needing explicit authority migration; linkage and two worker
adapter executables link. The owner now repairs these targets in isolated composition, without unchecked authority or
canonical activation, using turn `01a1eb9e-a843-70d8-8c19-e749c2c18171`. Previous lane released at 22:25:36; this new
bounded compile assignment owns the two-job lane. Active supported canonical SDK remains 4.35.

### Migration progress and denied scalar run: 22:41 UTC

The 4.36 scalar run was denied by automated per-command review at 22:28:48 before launch. No retry occurred; owner
resumed independent source migration. Root is resolving the exact authorization blocker. Compilation now passes for
four host examples, core profiling/examples/benchmark, three callback tests and four host fixture tests. The broader
all-target inventory still failed at 22:39:46; residual count is requested. No native target execution is established.
The owner remains active on the single two-job lane, with concrete compile evidence within the preceding two minutes.

### Caller migration checkpoint: 22:55 UTC

Configured 4.36 compilation now clears 26 of the original 29 failing targets. Tour stays standalone; session benchmark
and four attempt-envelope cases compile, not runtime-qualified. Remaining: long_session_memory (14 errors), core
library tests (94), host library tests (156). Owner continues these three targets on the two-job lane. Independent
review checks snapshot `41304eb5c47f08d47c49f3add7d32ffba0201e1b7dc45beb256aa18b33f907f8` for retained safety and
test value, including documented CLI-to-embedding changes. Scalar run remains denied before launch pending the
existing explicit approval question; no retry or canonical change occurred.

### All configured migration targets compile: 23:16 UTC

All 29 formerly failing targets now compile. Configured workspace all-target and four-crate checks exited zero;
warnings remain, not a strict-quality pass. Lane released 23:15:39. Patch 8 (`383c41fd5034f80db343212beb44439e5afd304e7a80fc82a70cbbeebcad1be7`)
is under independent review; warning cleanup, formatting and applicable strict Clippy are assigned in turn
`01a1ebcc-355a-73ca-81d9-ea9ff5fa18f3`. Host unsafe-code prohibition and private Meta constructor remain intact.
Nonempty trace destinations are rejected. Broader runtime workload records/pre-exec custody, reviewed temporary
projects and optional-service negative shim are still required. No native test or canonical integration occurred.

### Runtime-preparation corrections: 23:29 UTC

Checkpoint 10 corrects the source-options output path and includes private/server/ilean/IR sidecars in project
comparison; host all-target, strict Clippy and formatting pass. Patch `b175b7785dea0a99758cef9ba943344fb16d7e4c537fed26bb3e63a264281d0f`
is under review. Nine apparently Rust-only cases actually link 4.36 libleanshared before test entry and were NOT run.
A single representative 4.35 host-test provisioner has only placeholder/static evidence. Exact compile-only artifacts
and custody reuse are being prepared; no new native execution, permission bypass or canonical activation is assigned.

### Approved native results: 23:59 UTC

Both separately approved runs passed once. The 4.35 declaration-kind supervisor ran 23:55:23–23:57:30; actual transcript
shows one test passed, 77 filtered, body 16.08 s. The 4.36 scalar supervisor ran 23:58:44–47 and exited zero after 42,
termination acknowledgement, direct-child reap and five writer reacquisitions. Lane released 23:59:05. Prior denials
remain preserved. These are narrow fixture results, not broad 4.36 imports/kernel or production qualification.
Next work is isolated paired safety-source integration with warm ownership reconciliation; registration stays distinct,
exact-tree static gates must rerun, and production authority policy remains explicit. No canonical activation/release.

### Paired actor and policy checks: 00:12 UTC, November 18

Paired host `44e96533` and SDK `1ff65153` passed two cleanup and 27 actor receipt tests plus formatting/strict Clippy.
Independent re-review confirms original-timeout receipt preservation and live replacement capacity retention; controlled
fake-child evidence is not authenticated GO/native custody. The policy harness passed three tests at 00:10:31–37,
verified four source hashes and system-only linkage. Its modules remain unwired.
Provisioning turn `01a1ebfe-a4a8-7329-a3ce-f243f61fb804` now owns the two-job lane for isolated safe opener/installer
receipt and activation failure tests. ABI turn `01a1ebfe-bdc5-7096-a406-0aaff4c2c0d0` prepares worker custody contract
source-only, avoiding installer-file overlap. No live install, production activation or new native workload is assigned.

### Worker ingress and generation transaction: 00:20 UTC, November 18

ABI owner `6afca3dd-61c8-8191-beef-0e1235e410d2` completed turn `01a1ec03-7871-7331-bcdc-6b98a7eaeee5`:
SDK `3d93d5d17e01747262bc989bc061278531fd9898` and host `aa921357eb4c3c480e3ce054c8d7b442b1ccbf91`
pass targeted strict Clippy, formatting and SDK worker documentation. The feature-gated 4.35 worker compiled/linked
(SHA-256 `00b94d2e5ad7e2a3cb847740060bf24ca9f4167652caa69f5b89fd3b27b8b6a1`); two comparison contract tests
passed in a libSystem-only test binary. Evidence: `task-20/safety-pair/worker-ingress/compiled.sha256` and
`task-20/safety-pair/ingress-contract-{run,link}.log` under the existing November 17 Codex task directory.
No worker or guardian launched. Lane release: 00:18:58 UTC.

Independent source review verifies actual guardian-acknowledged readers precede every anchor open/reuse. Refusal can
already retain guardian aliases and update metadata, so it is not cleanup evidence. Exact non-UTF8 path identity and
separate guardian-executable provenance need explicit treatment before general production use. ABI follow-up
`01a1ec06-4dcf-708b-8650-d6f41c0c5006` was admitted around 00:20 UTC for source-only disposition, not another build.

Provisioning owner `6afca1a9-4f1c-8191-a757-cb2c906df0dc` prepared generation transaction patch
`acc6aeba8df7b9927af487ce3679a963e85552990ff4318a2b62ca61c697ec20` at
`task-15/host-native-policy-generation.patch`. Its previous safe-opener/policy harness passed five tests at 00:12:12–31.
The new transaction test is not yet executed: turn `01a1ec06-3b68-71d0-b2e6-077c586bf400` was admitted around 00:20 UTC
on the released two-job lane to compile/run actual temporary-directory staging/receipt/smoke/selector failure tests.
Admission is not a measured command start. Independent read-only review runs in parallel. Real installer and runtime
resolver wiring, protected receipts, actual writer/reader custody and scope-specific native smoke remain unfinished.
Old generations must remain available, and post-rename directory-sync failure is uncertain, not proof of rollback.

### Selector uncertainty and exact roots: 00:21 UTC, November 18

Provisioning completed six transaction tests and then seven with patch
`ac4736d1fde84f0094c02499e7d22fb473a7af838966b96abd89234d5159d26f`; actual command interval was
00:20:00–00:20:35 UTC, lane released. Injected directory-sync failure followed a real selector rename; new selection
and both generations' bytes were observed, not crash durability. Evidence: `task-15/host-policy-source/policy-harness/generation-run.log`.
Independent review requires protected-selector selection (pre-smoke receipt existence is insufficient) and completing
parent sync during writer-held reconciliation. Turn `01a1ec07-8213-717a-b171-10eaf32913a0` now owns the small nonnative
lane for a typed uncertain-activation result and reconciliation tests; actual installer wiring remains next.

SDK `21892a4fc073de61e2dc6c3f585d4b80e719ad7c` validates the full root union as UTF-8 before either acquisition path,
with a new regression for invalid byte sequences colliding under lossy rendering. Formatting/diff checks pass;
regression compilation/execution is queued, not passed. Independent review is active. Launcher contract now separately
requires guardian executable/closure verification and custody; writer-helper equality cannot supply that evidence.

### Integration prerequisites corrected: 00:24 UTC, November 18

Exact-root production-validator regression passed at 00:23:07–08 with libSystem-only linkage; targeted strict Clippy
and formatting passed, lane released 00:23:19. Evidence is `task-20/safety-pair/worker-ingress/root-identity.exmD6W/transcript`.
Provisioning reconciliation patch `6f2dc05a0dde0346aeb1e742b67721e12c89f20a03fd35ada6c57732d9b35eae` passed seven
pure tests at 00:21:44–46, but review found its typed token must also bind original receipt location and digest.
Correction and negative tests are assigned to turn `01a1ec09-d541-73b8-b0ea-300b7fe44380`, now using the released lane.

Inspection corrected an earlier assumption: current `install_one` has no per-toolchain writer acquisition, and
adjacent overwriteable `worker.json` is not independent release approval or a protected selector. Actual wiring
therefore remains blocked on a concrete minimal trust/coordination decision, not just compilation. The owner is
comparing reuse of existing installation provenance and canonical lock mechanisms, without assuming novel security
infrastructure. Ordinary/companion launch through `native_worker_child` and separate installer smoke both need the
chosen handoff; general explicitly authorized projects remain supported. Process-restart smoke-success evidence is
not supplied by the in-memory reconciliation token or a pre-smoke receipt.

### Local deployment decision and wiring: 00:26 UTC, November 18

Receipt-binding correction passed seven pure tests at 00:24:25–26 and independent review: reconciliation now binds
the exact original receipt location and digest, not just the generation pathname. The typed token proves trusted
callback success in that process, not arbitrary native smoke or post-crash success.

Coordinator accepted the cooperating same-user deployment model: reuse operator-selected release recipe or explicit
reviewed local checkout/runtime, existing installer/configuration and canonical locking. Verify output contracts and
closure against selected inputs; output hashes alone do not approve arbitrary code. Project admission remains separate.
This is not a hostile same-user security boundary and introduces no signing service or privileged deployment identity.
Provisioning turn `01a1ec0b-44c2-722c-b043-85be28fedebf` owns private installer wiring and bounded pure-test lane;
ABI turn `01a1ec0b-5d91-77a4-95f8-a10eeaa104d9` prepares ordinary/companion launcher handoff source-only.
Both were admitted around 00:26 UTC, not evidence of completed commands. One stable domain outside generations must
coordinate install, launch, clean and prune; launch custody lasts through actual retirement. Smoke must not deadlock
by requesting overlapping SH while its installer holds EX. Restart reconciliation and supported operation boundaries
remain explicit qualification gates; no production install or new native execution has occurred.

### Two-domain source and compile checkpoint: 00:36 UTC, November 18

Reviewed direction uses stable selector custody plus immutable generation custody, reusing existing SH guardian
loans rather than inventing EX transfer. Normal launch samples the selector, releases that read, acquires the complete
union and rechecks before payload; smoke selects the known staged generation without taking selector SH. This avoids
the installer EX/smoke SH deadlock only once the actual handoff is implemented. Bootstrap trust remains separate.

Host `1e7b847f` finite pre-exec hook and SDK `21892a4f` compiled with regression `bbf3b4cf`; parent/test link only
libSystem/libiconv. Nine warnings, no execution; lane released 00:34:51. Evidence: `task-15/custody-compile.dbI7Mf`.
Hardened regression now separates lock errors, observes non-zombie identities and explicit controller reap, and
preserves scratch on every outcome; it is uncompiled/unrun and under review. Source-only host `ac075f9a` adds shared
selector accessor and wrong-digest/zero-start tamper test. Composition remains uncompiled. Provisioning owns one
bounded compile lane for an agreed combined snapshot; ABI turn `01a1ec14-fb82-70fb-a3ed-a1a418b8777d` handles required
release expectation, bootstrap loader environment and mutable dependency custody source boundaries. No fixture run,
ordinary interactive startup qualification, public installer activation or deployment is established.

### Approved Lean-free runtime checks: 00:46 UTC, November 18

Exact `9916a317` composed fixture binaries passed separately approved commands: lifecycle at 00:45:41 (one test,
0.26 s) and tamper at 00:45:56–57 (one test, 1.37 s). Logs independently read under
`task-15/custody-final.ZOPWZX/{lifecycle-run.pEa3ko,tamper-run.m0kvgN}/test.log`.
Lifecycle asserts post-controller-loss generation exclusion and captured guardian/group retirement, preserving the
old selector and both byte strings. Tamper asserts verifier-specific rejection, zero payload starts and closed scope.
Lifecycle scratch remains preserved. An extra process census was sandbox-blocked, so closure evidence is the test's
identity/group observations, not an independent census. Rust executables are system-linked; lifecycle uses Homebrew
Python, and private SDK IPC also uses /tmp. No Lean or installed production worker was exercised. Lane released 00:46:03.

Interactive continuation remains source design: existing Rust Child/control sequence does not survive exec as a Rust
value, watchdog must not start a second guardian, pre-ACKed reads must not be reacquired against pending writer demand,
and one absolute startup deadline must span bootstrap/adoption. Existing ABI owner continues a narrowly typed Unix
handoff and harmless framed-worker plan; finite test success does not qualify that unfinished mode.

### Continuation and recipe wiring checkpoint: 01:10 UTC, November 18

SDK `1c45c6dd` passed worker-parent all-target checking and six isolated continuation tests with system-only linkage;
lane released 01:08:27. Single wire clock, bounded ACKs, explicit retain sequence and receiver CLOEXEC corrections are
implemented. Production adoption remains disconnected: generation custody across exec, pre-concurrency receipt and
exclusive child reaping need real launch wiring. No worker/guardian process ran in these tests.

Provisioning has bounded operator checkout recipe selection and specific unsupported selection errors. The selected
audit index is digest-checked; decoding/binding it into transaction inputs and the guarded fixed smoke driver remain
unfinished. Turn `01a1ec33-9901-7078-ba30-411450b2c4c4` owns pure tests/compile; ABI turn
`01a1ec33-abb9-70e8-aff4-4b3ece79914c` owns source-only actual launch wiring. Public install remains stopped before mutation.

Twelve mutually owner-cleared incremental caches (2.78 GiB) moved recoverably to
`~/.Trash/lean-incremental-20261118-0104` at 01:05:16; no binary/source/evidence removed. This does not reclaim disk space
while Trash is retained. Build hold ended; current targets are reused rather than copied per revision.

### Installer handoff and cleanup qualification: 01:23 UTC, November 18

Cleanup patch passed 40 installer tests, including independently reviewed symlink rejection, historical generation-ID
reservation and busy-generation preservation; stage EX spans directory syncs. No real artifact removal occurred.
Lane released 01:21:23. Cumulative source handoff: `task-15/host-installer-abi-handoff.md`, with exact patch/base/hashes.
Public install awaits the real admitted-handle entry, then fixed theorem success, graceful exit zero and custody closure.

ABI owns the two-job lane and actual continuation/entry wiring. Independent review found continued first-open errors
after admission validation, authority checking or anchor creation still returned to the loop; a latch prevents another
open but does not provide prompt retirement. Terminal cleanup preserving the primary error is assigned before activation.
Installer owner is waiting on that concrete entry, not running another scaffolding task. No production or Lean run.

### Actual exec wiring and remaining qualification: 01:42 UTC, November 18

SDK `d7643c9d` / host `d2c89d35` wire the guardian-issued endpoint across exec and concrete staged smoke operation.
Configured targets and seven isolated tests pass; framed worker/bootstrap compile with system-only linkage, but have
not run together. ABI turn `01a1ec4d-b807-725a-8bd0-c88f03bd963a` was admitted around 01:37:59 (UUID timestamp),
preparing the existing-supervisor harness with independent roots, one guardian/handshake, PID continuity and retirement.
This is elapsed assignment time, not measured compute time.

Provisioning turn `01a1ec51-2588-777f-bbd8-b69d5bea6b8c` was admitted around 01:41:43 for committed-source staging and
custody wiring. Controlled-input patch remains source-only: review found approved Init file hashes must survive into
the under-custody verifier, canonical containment and complete import closure are required, and a caller-provided
snapshot is not a controlled committed build. Build scripts/toolchain dependencies require explicit trust/binding.
Public build/install remains disabled. Mac check 01:42:08: 57% memory free, load 2.99/2.67/2.74, 31 GiB disk free.
No further large cold build without capacity preflight; one qualification target, no automatic duplicate retry.

### Controlled-build integration checkpoint: 02:06 UTC, November 18

Pinned SDK `d7643c9d` / host harness `3404c4b3` passed the actual Lean-free cross-exec happy case
at 01:49:22–25 and wrong-grant case at 01:50:11–13. Both reported confirmed closure without intervention;
project/generation writer reacquisition passed. The negative started the worker and refused before handshake,
not before process start. Evidence is under `task-20/safety-pair/worker-ingress/framed-compile/` and retained
`/private/tmp/cross-exec-4mgmKd` / `cross-exec-A2EnM0`. These results do not qualify newer source or real Lean.

Provisioning passed 15 pure artifact-handoff tests at 01:52:21–28; synthetic receipts do not qualify native
handoff. It composed installer/runtime source once in `task-15/controlled-build-composition.5LwQUV/host`,
base `3404c4b3`, recording input and composed hashes alongside it. No composed build or native run occurred.
Independent review found a caller that built then errored, dropping its returned artifact: installation must
fail before spawning until live artifact ownership reaches activation. The provisioning owner is correcting it.

ABI v2 source removes the arbitrary command callback and adds acknowledged-loan Init inventory verification.
Its mandatory graph check currently rejects the Lean-free framed fixture. Distinct independently selected
operation verification is being corrected; production verification must not acquire a fixture bypass.
Queued-writer/uncertain-retirement regression, combined compilation, stock policy and activation remain open.

Active owners: ABI `6afca3dd-61c8-8191-beef-0e1235e410d2`, turn `01a1ec66-d5e2-7263-bab8-472f914f9f0d`;
provisioning `6afca1a9-4f1c-8191-a757-cb2c906df0dc`, turn `01a1ec66-c49d-7228-b43d-aa8e85aa5b33`.
Both turns were admitted during this checkpoint; they own runtime and installer files respectively. Independent
source review continues. Lane was idle at the source handoff, with 29.9 GiB reported free. Maintain a 25 GiB
reserve, reuse the existing target, and require fresh capacity/growth preflight before compilation. No production
installation, Proofs/Mathlib build or cache deletion occurred.

### Qualification correction checkpoint: 02:43 UTC, November 18

The composed host library passed checking at 02:12:58–02:13:03. Promotion fixture binaries linked at
02:24:40–02:25:08; independently verified hashes and system-only load commands are in
`task-15/controlled-build-composition.5LwQUV/promotion-artifacts.sha256` and `promotion-load-commands.txt`.
The positive fixture then FAILED once at 02:27:15–02:27:41, exit 101, before guardian ACK. No negative ran.
Evidence remains in `selections/queued-writer/test.log`; this is not a custody or promotion pass.

Source diagnosis: a private operation marker failed generic canonical-executable validation before helper spawn;
the contender timeout hid that refusal. A correction uses the canonical helper plus a reserved argument, retains
validation and reports both outcomes. Independent review additionally requires wrong-executable/reserved-marker
rejection. ABI turn `01a1ec88-19ea-72bc-a39c-bbcfe52f32be` owns that addendum. Provisioning has rechecked
source/selections and is waiting, not compiling. A fresh rebuilt/pinned fixture is required; never replay the failed
case blindly. The read-only census found no fixture-matching process or socket directory but captured no guardian
birth/group identity, so it does not establish the failed invocation's retirement.

Separate stock Init patches now explicitly compare project-prepared manifest path/bytes and image identities to
release-audited evidence before ACK. The release-side companion populates the snapshot from already verified bytes.
These remain source-only. Actual transitive loader closure, ordered imports under custody, independent controller
pin and private activation qualification remain gates; public installer still refuses before spawning.

Owner-cleared old cache cleanup was performed by the capacity coordinator, preserving active target and evidence.
Latest provisioning disk check: 56.08 GiB free; retain the 25 GiB reserve. Formatter's separate bounded lane has
released. No production installation or Proofs/Mathlib build occurred.

### Private Init failure and diagnostic correction: 04:33 UTC, November 18

The corrected controlled-build fixture passed its positive case at 02:46:56–57 and injected-unwind negative
at 02:48:58–59. These are bounded fixture results, not actual failed OS retirement or stock Lean qualification.
Evidence remains in `task-15/controlled-build-composition.5LwQUV/`.

Private stock Init uses separately reviewed prebuilt provenance, not a claim of reproducible Cargo output.
The reviewed controller digest is `5110db7051a5cb6fbe3d67e2c64b8d860941ca9156f0569845d7945e4da58435`;
its matching worker is `4cbc140fc31eab72b0eb92c2172a858e2637e83db5dbbcca95a05b218e4e42f2`.
Fresh shim-owned canonical Lean 4.35 runpaths passed static loader review. Static closure is not runtime success.

The first real private Init qualification FAILED once at 04:26:18–04:26:37, exit 1, with
`guarded install smoke did not complete and retire cleanly`. No active selector was published. Evidence is in
`task-15/shim-own-runpath.ZdDYpQ/private-init-qualification.sgIft3/`, including the independently selected audit
`c37312105ec9a2bea54939c08ff262183c47022a908b5f3bc721658b49e86611`. The controller discarded the detailed
supervisor result and child streams, so the failed lifecycle predicate cannot be reconstructed. Empty controller
stdout is not evidence about worker stdout or typed declaration evaluation. Later absence of recorded processes
is not proof that retirement was confirmed during the invocation. Preserve the failed generation; do not replay it.

Two diagnostic patches are prepared but not yet compiled or tested: provisioning report persistence
`a0ea941071d6aedff7f1950475fcf4f9325184346a0088d4439d45f66327b798` and runtime capture preservation
`3a749cc79d942d7395ab707e4590883e88a685ee2aec7e1f2bef090762f6d6ed`. Reports are bounded, create-new and
mode 0600; overflow/cleanup-expiry capture retains bounded tails and nullable unknown fields. Report-write failure
does not change the original acceptance decision, so qualification must separately require the diagnostic report.

Provisioning owner `6afca1a9-4f1c-8191-a757-cb2c906df0dc`, turn `01a1eced-26a4-72b2-b8e5-bd785d853e75`,
was admitted during this checkpoint to compose once and run pure tests in the existing warm two-job lane after
capacity preflight. ABI owner `6afca3dd-61c8-8191-beef-0e1235e410d2`, turn
`01a1eced-367d-7360-ac5f-fc97b387d3a0`, reviews composition without duplicate builds. Independent source review
is assigned separately. Admission is not measured command start. Next gates: reviewed composition and pure tests,
new controller identity and matching worker, then a newly reviewed qualification packet. No native rerun is admitted
by this checkpoint. Public installation, Lean 4.36 runtime support and Proofs integration remain unqualified.

At 04:36:03–04:36:04, all five final exact Lean-free diagnostic tests passed. Independent source review
verified the final composed hashes and corrections: final stderr polling, preserved first interruption reason,
bounded report-error fallback, and accepted-result/report-write-failure behavior. Late stderr overflow now
intentionally stops acceptance even with exit zero. Evidence: `task-15/stock-init-smoke-diagnostics.mXHGxw/`.
The revised controller built once at 04:37:14–04:37:27, digest
`74cc0da1e2997a95ef25ecb8f81edeb4bd41ae88e8e89e350cc7170bd2c69961`. Owner-recorded static inspection is
system-only with no RPATH or enabled features; independent artifact selection is in progress before matching-worker
build. Disk finished at 49.33 GiB free and the lane was released. Neither candidate nor native qualification ran.

### Domain correspondence correction: 05:05 UTC, November 18

The diagnostic qualification ran once at 04:54:59–04:55:16 and failed with child exit 1:
`install smoke selection differs`. Its retained generation-NPtLZw report confirms startup and scope closure,
without intervention or output truncation. The theorem operation was not evaluated and no selector was published.
Both owners identified the exact mismatch: the worker omitted the selected Lean sysroot from its project-domain
request, while the controller and guardian receipt included it. No installer or controller change was required.

Reviewed worker patch `6364bf5ffe4bb299b475f50e476cc1f3b274006ad0d4ae04c4d01acd4850dce4` binds the selected
absolute sysroot to the launch value; exact authenticated request comparison and all-domain custody checks remain.
The extracted production-helper regression passed at 05:00:25–05:00:26. It tests domain correspondence, not guardian
refusal. The corrected worker built in 21.18 seconds, SHA-256
`5613508f01292700179cae533cce1fedd3724df306cfe6db25945c895eaf0ef9`, with unchanged controller pin and empty
features. Fresh audit `f1eef2d0adcc5ff3a86f953eb65afb7ea6b2d5387a57fab979d4807be18ee011` is under independent
selection before exact-command approval. Evidence is in `task-15/stock-init-domain-correspondence.E24IXs/`.
Both failed generations remain frozen. No successful native Init qualification or public installation is claimed.

At 05:06:18–05:06:36, the independently selected corrected private Lean 4.35 Init qualification PASSED once,
exit zero. `private-corrected-init.8Acld3/run/receipts/generation-4zGW8y.smoke-diagnostic.json` records accepted
`Nat.add_zero`, kind theorem, untruncated statement `∀ (n : Nat), n + 0 = n`, confirmed closure and no intervention.
Private selector, protected receipt and staged worker digest agree. The reconciliation census found no recorded
worker/guardian PID or process group. This is private declaration inspection, not a kernel recheck, general-project
qualification, controlled-build reproducibility, Lean 4.36 qualification or public deployment. All prior failures remain
preserved. Independent result review is underway. ABI turn `01a1ed0e-44b3-70be-86c5-95a053fa4a02` and provisioning
turn `01a1ed0e-5a68-73f6-975e-9b73873412c4` were admitted to concrete source integration and next-gate preparation,
with SDK/runtime and installer ownership separate; no further native execution or public activation was assigned.

### Private 4.36 runtime and integration checkpoint: 06:12 UTC, November 18

The isolated installer accepted the 4.35 positive at 05:27:00–05:27:21 and rejected the crossed-manifest case
at 05:28:30–05:28:51. Independent review verified the exact pre-ACK refusal, observed owner closure, and unchanged
original selector, receipt and staged-worker bytes. These are sequential isolated cases, not concurrent production
registry qualification.

The 4.36 shims built at 05:50:41–05:51:02 and passed independent static selection. Controller
`bfae7e09fe2178beb2a4ff3c57d9516f8cd9833cede54107cec7223681a45cb6` and worker
`1fa9975bcc1603dae7393747c980fc192fe75b7b1c48b53f2cb621c15a48cded` were independently selected against
archive-matched off-elan inputs. The private Init invocation began at 06:08:02; generation-fmlBAp was reported
at 06:08:21. Its diagnostic records the typed theorem, worker exit zero and confirmed closure without intervention.
Receipt, policy, selector and staged bytes agree; later recorded process identities were absent. Independent source
review confirms generation output occurs after successful selector-parent synchronization. Thus the private application
transaction and worker runtime succeeded. The zsh wrapper then failed on read-only variable `status`, losing the
controller OS exit status and exact finish time. Do not record the overall command as exit zero. Evidence is preserved
without replay. A replacement wrapper passed harmless exit-0, exit-37 and overwrite-refusal checks.

Evidence root: `task-15/stock-init-protected-adapter.oRkagk/stock-436-pair.1ezJiz/`. Immutable private SDK source
archive `sdk-436-stock-source.tar`, SHA-256 `a36e8880acc1a0b4edc6792a66d3e884e5da816f2b8f472de372fcdd3a6adcd6`,
is handed to downstream isolated testing; it is not a registry or canonical release.

The actual main/semantic broker remains under integration. Combined source compiled and focused cancellation,
expiry, manifest, path-containment, selected-environment and project-issuer tests passed; controlled actor tests
are not the full installed guardian route. ABI owns the concrete project issuer and framed-route packet; provisioning
owns ancestry-aware installer/adapter integration. Protected project closure, full tracked launch and replacement,
canonical gates and release remain unfinished. No public activation or Proofs/Mathlib build is claimed.

### Saved SDK-1/SDK-3 acceptance addendum: ordered roots and inherited loader inputs

Source inspection on 2026-11-18, not a runtime qualification: Lean 4.36 `Lean/Util/Path.lean`
`SearchPath.findRootWithExt` chooses the first search root containing top-level `A/` or `A.olean` for `A.B`;
`findWithExt` then constructs that root's `A/B.olean`, rather than searching later roots for the full filename.
Lake `Lake/Config/Env.lean` retains the initial shared-library search path and combines it with Lean's paths;
`Lake/Config/Workspace.lean` adds workspace library paths for `lake env` (macOS uses `DYLD_LIBRARY_PATH`).
The inspected canonical SDK and isolated upgrade adapter Rust sources contain no `lake setup-file` invocation.
`lean-rs-host/src/host/lake.rs` constructs search paths from project/build/package layout; worker-parent
`supervisor.rs` inherits environment and applies configured overrides, rather than clearing it. Sysroot discovery's
`lake env` probe is not per-file import/plugin setup. This inspection does not prove every possible external wrapper absent.

Required focused acceptance, owned with SDK-1/SDK-3 and host integration (planned, unrun; no paused work resumed):

- Use ordered roots R1/R2 where R1 contains `A/` but lacks `A/B.olean`, and R2 contains a valid `A/B.olean`.
  Compare selected resolver behavior with the pinned Lean runtime: preserve the first-root failure, not an SDK-invented
  fallback. Reverse roots and test the top-level `A.olean` case. Never sort/deduplicate roots in a way that changes
  resolution; preserve logical source roots separately from compiled import roots.
- Between preparation and a deferred import, have a controlled concurrent builder introduce/remove R1's `A/`, replace
  a transitive olean or native plugin, or rebuild a path dependency. Require the selected protected generation or an
  explicit refusal/NeedsBuild, never silently read a different graph. Record the actual resolved files and generation,
  not just before/after hashes. An uncooperative stock Lake writer is outside SDK-only lock protection.
- Run both ordinary launch and launch under `lake env` with conflicting inherited `LEAN_PATH`, `LEAN_SRC_PATH`,
  `DYLD_LIBRARY_PATH`/fallback/injection settings and their supported-platform equivalents. Record effective ordered
  import paths, initializer/plugin selection and actual loaded-image dependencies; require selected inputs or explicit
  rejection, with no unapproved shadow library initializer running. Do not claim environment sanitization from
  configured overrides alone. Include a trusted selected plugin positive and a shadow/missing plugin negative.
- If file-specific Lake setup is introduced, capture its ordered paths, options and plugin/dependency outputs as inputs
  to the same prepared handle and revalidate under its chosen stability mechanism. Do not run Lake setup/build after
  holding read locks that its writers need. If unsupported, document the subset instead of claiming full Lake parity.

Use existing loader regressions and focused host import tests; exact test placement follows the chosen SDK mechanism.
This addendum requires no new service, custody authority, fixture campaign or native execution now.
