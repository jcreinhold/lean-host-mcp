# Rust stage state rules

Status: **unwired executable reference kernel**, not a running execution service. The source is `stage_kernel.rs`; its
focused tests are `stage_kernel_tests.rs`. Neither file is compiled into the server or called by current proof work.

The kernel keeps one current warm scope and its stage history together. This avoids making callers coordinate separate
stage objects to prevent an old message from changing new work. It uses only the Rust standard library. It has no actor
runtime, queue, clock, process calls, signals, lock operations, storage or transport.

## What it checks

An attempt ID includes a store ID, acceptance epoch and nonce. A stage key adds an explicit stage number. Each accepted
stage keeps immutable intent bytes and its original scope ID, generation and owner. The owner ID names a logical
incarnation; it is neither a PID nor a capability to signal a process. An equal retry returns the original binding,
including after closure or result expiry. Changed intent fails without changing state.

Only one stage can occupy the current scope at a time. Authorization alone does not launch anything. GO consumes that
stage's gate before returning one `Launch` effect. Duplicates, failed spawn, result expiry, closure and generation
replacement cannot reopen it. Cancel before GO seals the gate. Cancel after GO requests a stop; a previously issued
launch may still race it. Cancellation never claims closure.

The kernel keeps these facts separate:

- Launch gate: awaiting authorization, authorized, consumed or sealed.
- Observed spawn: absent, spawned or failed.
- Stage resources: open, stopping or closed. These labels describe the stage association, not ownership of a real
  artifact lock or proof of OS cleanup.
- Result retention: pending, retained, acknowledged or expired. A result ID is opaque history, possibly a prelaunch
  error; recording it does not prove execution or semantic success. Expiry retains an identity record, preventing
  resurrection.

Results and ACKs may update their original history after generation replacement. They cannot release resources or alter
the current stage. Conflicting result or spawn reports fail unchanged. Duplicate reports cannot restore expired data or
reset acknowledgment. ACK/expiry require the known result ID; expiry need not wait for ACK. Reusing the same result ID
across stages does not weaken stage isolation.

Stage closure and full-generation closure are distinct inputs. A late stage callback cannot close a newer stage or
generation. Replacing a generation requires observed full-generation closure. The kernel retains the highest generation
for each stable scope ID, including empty generations: `A5 -> B0 -> A2` fails, and an owner change cannot reset that
record. This matches stable scope reuse in the proposal without imposing a second global UUID-freshness policy.

## Small API

`Kernel::new`, `accept`, `apply`, `observe_generation_closed` and `replace_generation` change state. `scope` and `stage`
return read-only snapshots. The module is crate-internal Rust data, not an RPC schema or a public MCP API.

Every stage event supplies its original `StageBinding`. Events cover authorization, GO, spawn observation, cancellation,
stage closure, result observation, ACK and expiry. Effects are `None`, `Launch(original_binding)` and
`RequestStop(original_binding)`. A repeated cancellation emits no second stop effect. Returning an effect does not
ensure delivery: the future runtime must perform or reconcile it. This kernel is not a durable outbox or a reliable
channel.

## What the future runtime must establish

Observation values are trusted inputs to this component. A closure input must mean **all protected work has stopped and
every pending launch is sealed so it cannot run later**. A quiet process census alone does not establish that. A late
spawn report describes original historical work, not a newly permitted launch.

The runtime must authenticate the producer and original binding, durably bind intent before dispatch, deliver effects
without unsafe replay, preserve pending uncertainty, and obtain real launch/closure observations through qualified
native ownership. No Rust enum supplies those facts. Starting a fresh kernel forgets its history; this code provides no
safe restart or restoration API.

The maps retain stage and generation identity records and can grow. A real store needs bounded retention that preserves
the required non-replay identities. This step supplies no admission, pruning, lease or capacity policy.

The kernel guarantees at most one returned `Launch` effect per accepted stage within its in-memory lifetime. It does not
prove at most one OS spawn across crashes, physical exclusion, eventual cleanup, authenticated messages, reliable effect
delivery, SQLite correctness or completion of the execution-service redesign.

## Model relationship and review

The tests exercise failure cases also checked in `Stages.tla`: failed-spawn replay,
owner/generation mismatch, changed intent, rebinding after replacement, old cancellation of new work, result-as-idle,
ACK of another result, and old cleanup of another stage/generation.

The Rust kernel accepts matching historical result/ACK/spawn facts after replacement while isolating current work.
Retention preserves result identity and ACK/expiry state; a late duplicate cannot restore removed bytes.
The TLA+ stage model checks historical result/ACK isolation and one-use gates, but it does not cover every Rust API
or prove that the implementation follows the complete model.

The Rust tests include 48 ordered pairs of old controls across pending, running and replacement cases. They deliberately
give old/new stages the same result ID. These are targeted regressions, not exhaustive exploration. The larger combined
TLC check remains incomplete as recorded in the spec README. Independent agents reviewed the API, implementation and
observations boundary before commit.

Recorded validation, 2026-11-17: all 25 Rust tests passed, including 48 ordered late-control pairs. Six deliberately
broken copies compiled outside the checkout and each failed its targeted regression: wrong-stage stop, old result
declaring idle, old ACK expiring a current result, old stage/generation cleanup changing new work, and a failed spawn
reopening its gate. These checks test the assertions; they do not establish exhaustive coverage or a runtime refinement
proof.

## Run without building the server

From the repository root, write test binaries only to temporary storage:

```sh
rustc --edition=2024 --test -D warnings specs/native-execution/stage_kernel.rs \
  -o /tmp/lean-stage-kernel-tests
/tmp/lean-stage-kernel-tests
rustfmt --edition 2024 --check specs/native-execution/stage_kernel.rs
clippy-driver --test --edition=2024 -D warnings \
  -W clippy::all -W clippy::pedantic -W clippy::nursery \
  -W clippy::arithmetic_side_effects -W clippy::unwrap_used \
  -W clippy::wildcard_enum_match_arm specs/native-execution/stage_kernel.rs \
  -o /tmp/lean-stage-kernel-clippy-tests
```

No Lean fixture, Mathlib build, server restart or live process intervention is needed. This portable standard-library
kernel has no platform-specific test skips; that does not extend the separate macOS custody qualification to other
platforms.
