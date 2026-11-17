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
