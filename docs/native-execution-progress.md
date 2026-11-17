# Native execution progress

Snapshot: 2026-11-17, 19:00 UTC. Maintainer: tooling coordinator. This is the current gate ledger, not a second design.
Requirements remain in [lifecycle §16](native-execution-lifecycle-design.md#16-remaining-implementation-and-acceptance-gates).
Update this file after reviewed results, ownership changes or build-lane transfers; do not mark a gate complete from a
session ending. Distinguish source review, executed tests, canonical integration and deployment.

**No replacement production service or deployment is qualified.** Overhaul work proceeds in parallel with Lean 4.36;
only shared source ownership, immutable SDK compatibility and substantial builds impose dependencies. Proof Lean work
remains paused. Mathlib and its caches are outside these tasks.

## Current gates

| Gate | State and evidence | Owner; next action or blocker |
| --- | --- | --- |
| Frozen scope and cleanup | **Unfinished.** Earlier parent-alive CONT evidence is narrower than service recovery. Current no-extra-lock integration has zero native executions. Review found guardian startup READY failure can lose child authority, and outer cleanup-error handling lacks rescue. | Guardian session below: repair both paths, independently review, then qualify normal CONT, omitted CONT and early loan loss with the actual SDK loan as sole protector. No observed surviving guardian is claimed. |
| Registry recovery and admission | **Partial canonical fixes.** SDK `120767d` separates record retention from compute occupancy; `4ae59cd` drops ungranted writer candidates before owner waits. | Coordinator/guardian: replacement-service token/boot admission, publication, recoverer arbitration and own-crash behavior remain unresolved. Current absence and unknown historical result remain separate. |
| Warm generations and capacity | **Isolated candidate tested, not integrated.** Canonical dormant pin is SDK `a24394e`. Paired candidate has 25 dispatch and 70 supervisor passes (14 overlap), eight compiled mutation controls, Clippy and shutdown harness. Independent review verified seven checkpoint hashes and logs. | Warm session: address decoder-panic unwind bypass of error sealing. Then review breaking refusal API with host retirement, preserve first error and capacity until actual closure. Real custody, durable handoff, closure/rearm and production activation remain open. |
| ABI safety prerequisite | **Incomplete isolated migration.** SDK checkpoint `352c4dadbb2e9a9d5a3bde5b88e925636d8de7c6`; preceding compile-fail checks passed, native valid-object tests were compile-only, host checking failed on unfinished adapters. | ABI session: finish bridge/caller migration, consolidate import-lock domains without losing bounded/reentry semantics, establish allocator support contract, then test coherent SDK/host family. Not a requirement to finish every 4.36 project first. |
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
- Formatter now has the lane. ABI, guardian and warm repairs/reviews proceed without substantial builds.
- Guardian qualification follows formatter only after cleanup review and explicit lane admission; ABI/warm test needs
  are coordinated by readiness. A lane grant is not permission to deploy or run proof/Mathlib builds.
- Semantic 4.36 Lean build/tests passed on an isolated archive of `333c219`; its Rust/runtime qualification still needs
  an immutable SDK. SDK audit `e9baea03` covers 93 raw declarations and finds two missing string exports. These are
  compatibility inputs to the ABI family, not reasons to halt independent lifecycle work.
