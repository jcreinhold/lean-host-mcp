# Native execution progress

This planning document has moved to the single cross-repository
[Lean tooling stack](https://github.com/jcreinhold/proofs/tree/main/docs/projects/lean-tooling).

[Canonical dated evidence](https://github.com/jcreinhold/proofs/blob/main/docs/projects/lean-tooling/evidence/tooling-progress-history.md).
The stack roadmap, prompts and state own current decisions and execution order. Historical statuses are not current
implementation or deployment claims. Repository API, operations and formal contracts remain in their owning repository.

## Host status at consolidation (2026-11-18 17:21 UTC)

Production `native_service` is not wired. SQLite is a Unix dev-dependency for unwired contract tests, not the deployed
job store. Selected Entry/proof-state/semantic and installer fixtures passed earlier; they do not establish general
project admission or complete lifecycle/fault behavior. Saved implementation patches remain checkpoints, not deployment.
No host source/build/service changes were made by this consolidation. Existing observations through 17:12 are preserved
in the canonical dated evidence linked above. The new documentation commits are local until separately pushed.
