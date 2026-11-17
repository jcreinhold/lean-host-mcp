# Native execution progress

Snapshot: 2026-11-17, 20:09 UTC. Maintainer: tooling coordinator. This is the current gate ledger, not a second design.
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
| ABI safety prerequisite | **Incomplete isolated migration.** SDK checkpoint `352c4dadbb2e9a9d5a3bde5b88e925636d8de7c6`; preceding compile-fail checks passed, native valid-object tests were compile-only, host checking failed on unfinished adapters. | ABI session: unified import transaction has five standalone passes; installed-header allocator probe accepts three old toolchains and rejects non-mimalloc fixtures. Host integration and runtime/header pairing remain unverified. Bridge source now covers 23 of 51 rows; new bindings and host import/pool/query migration remain uncompiled. Independent source review of both task-20 patches found no lock/signature defect; eager shim exports and new target C-tool requirements need compatibility qualification. Third-patch review found unbounded attestation-key diagnostic history and independently pairable prepared metadata/loaded image. Fourth hashed patch bounds diagnostic shapes to 256 and binds custody/image loading; standalone history stress passed, source fixes are under review and native mismatch rejection remains untested. Review also found three explicit Lean Unit arguments incorrectly called as nullary; correction and generated-C signature checks block affected native calls. DOCS_RS=1 cumulative crate check failed with 17 host migration errors; fifth hashed patch migrates these plus worker loader/four caches and corrects three explicit Unit arities. Source review and repeat compile are pending; next-lane-check.sh is syntax-checked for documentation/configured worker-child checks and five regenerated C signatures. No compiler/native call ran in that preparation. Fifth review confirmed Unit fix but found worker snapshot/ordered-path admission and native module-cache environment identity gaps. Sixth hashed patch repairs snapshot/path admission and native cache scope identity; independent review pending. Configured Lean 4.35 worker/host targets now compile, and regenerated C confirms the five affected signatures; native regressions remain unrun. Not a requirement to finish every 4.36 project first. |
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
- Guardian released the lane after harmless qualification. ABI now owns the two-job lane for documentation/configured
  compile-fail/configured-version and generated-C signature checks after releasing its successful 4.35 campaign. Production authority installation and combined guardian validation remain open; real freeze/native ABI gates are unchanged. Guardian native qualification remains blocked on safe containment regardless
  of lane availability; source repair continues. A lane grant is not deployment or proof/Mathlib authorization.
- Semantic 4.36 Lean build/tests passed on an isolated archive of `333c219`; its Rust/runtime qualification still needs
  an immutable SDK. SDK isolated `750846a1c27783fbc7b71c652fe596aa36635b52` removes the two missing string exports coherently; all 91 retained exports pass bounded audits across four inspected Apple Silicon libraries, including 4.36. Independent ABI review confirmed consistency; integration requires audit `e9baea03` followed by removal `750846a1`, not removal alone. Cargo/Lake qualification and runtime/header pairing remain open. These are
  compatibility inputs to the ABI family, not reasons to halt independent lifecycle work.

Coordinator checks completion callbacks first and checks active lanes about every ten minutes. Twenty minutes without
new evidence triggers a specific progress/current-command query, not an automatic kill or duplicate session.
