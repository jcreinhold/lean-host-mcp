# Native execution progress

Snapshot: 2026-11-17, 20:41 UTC. Maintainer: tooling coordinator. This is the current gate ledger, not a second design.
Requirements remain in [lifecycle §16](native-execution-lifecycle-design.md#16-remaining-implementation-and-acceptance-gates).
Update this file after reviewed results, ownership changes or build-lane transfers; do not mark a gate complete from a
session ending. Distinguish source review, executed tests, canonical integration and deployment.

**No replacement production service or deployment is qualified.** Overhaul work proceeds in parallel with Lean 4.36;
only shared source ownership, immutable SDK compatibility and substantial builds impose dependencies. Proof Lean work
remains paused. Mathlib and its caches are outside these tasks.

## Current gates

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

## Active ownership and evidence locations

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
