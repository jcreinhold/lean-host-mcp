# Native tests for the stage adapter

Status: **test-only prototype; the running server does not use it.**
The [state kernel](stage-kernel.md) checks stage identity and launch history. These
macOS tests connect that kernel to real, disposable child processes through the
existing SDK. They test a narrow execution path, not the full service redesign.

The entry point is
[`stage_runtime_contract.rs`](../../crates/lean-host-mcp/tests/stage_runtime_contract.rs).
Its three helper modules separate native process ownership (`fixtures.rs`), the
small adapter (`runtime.rs`), and the six test cases (`cases.rs`). They add no
production module, dependency, queue or actor framework.

## The exact process and lock arrangement

Each stage uses a **new process group that ends when the stage ends**. This is
narrower than a warm Lean worker handling several requests. The adapter holds the
actual root child through `OwnedCommandGroup`, its original stage/scope binding,
and the sole private stdin sender. It holds no artifact-lock copy.

The root starts a separate SDK guardian in the same session.
`LeanWorkerNativeScope` and `LeanWorkerNativeScopeLease` register and acknowledge
that startup. Separately, `GuardianHandle::retain_reads` transfers the shared
read-lock descriptor and waits for the guardian's acknowledgment. The root then
drops its own lock copy. An adoption barrier prevents even guardian startup before
the adapter owns the actual child. The guardian remains the lock holder; the outer
test runner has no hidden copy. Locks close when ownership ends. The fixture never
explicitly unlocks a live lock or changes its shared/exclusive mode.

Preparation creates that root and guardian, **not the target task**. The kernel
consumes its one launch permission before the adapter takes the sole sender and
writes GO once. The root reads GO once, then starts the dummy target. The target
closes inherited descriptors and writes its own start record and heartbeat.
Repeating a request uses the original recorded binding and cannot create another
root or sender. No guarantee survives a fresh, empty adapter after a crash.

The adapter builds callbacks from private native observations. Each captures the
original stage, scope generation, logical owner and actual root birth identity.
It does not obtain those identities from whatever task happens to be current
when a delayed callback arrives. These are local test boundaries, not a claim of
an authenticated network protocol.

## A result and cleanup are separate facts

The target can return 42 while its root and guardian remain alive. That records
a result and leaves resources open. It cannot permit replacement work.

Cleanup requires all three observations: the SDK's owned-group wait has completed,
the sole GO sender is sealed, and the original lease reports its guardian dead.
The adapter caches these original closure facts; it does not later inspect an old
numeric process-group ID and treat that as new authority. A missing observation
or OS error leaves cleanup unconfirmed. Direct child exit alone is insufficient.

The current native context stays recorded until the closed generation is
replaced. A new stage cannot reuse it. Old stage results and acknowledgments may
update their own history; they cannot stop or close the replacement stage. A
callback for an old whole generation becomes a no-op at the adapter boundary.

## What the tests check

| Case | Required observation |
| --- | --- |
| Equal preparation retry and repeated GO | One real root and one leaf start |
| GO written, reply lost | Original child reconciled; no repeated task |
| GO dropped before writing | Receiver sees EOF; no target, invented spawn failure or renewed permission |
| Stop delivery or cleanup observation lost | Cleanup retried; no premature closed state |
| Old stop, result, cleanup, ACK and expiry after replacement | Old stop leaves the new target alive and writing; old callbacks preserve its state and retained result |
| Deliberately recreated receiver for the same stage | Two real leaf starts; the single-start check rejects the broken retry |

The loss switches simulate effect or observation delivery failure. Process creation,
GO delivery, target output, child waits, lock transfer and cleanup use real APIs.
The result is dummy output, not a Lean proof verdict.

In two positive cases, an independent exclusive-lock contender proves that it is
blocked **before** cleanup starts and keeps trying during cleanup. At first
acquisition it checks the protected group and appends a marker to the target's
heartbeat file. Success requires group absence and zero target bytes after that
marker. Test completion must precede the fixture safety deadline; emergency
cleanup cannot count as success. Failed cases preserve their temporary files.

Separate broken copies tested the checks themselves. Removing the old-stop
binding check actually stopped the new target; its heartbeat stopped advancing.
Routing old cleanup to the current binding cleared the new active stage. Both
copies compiled and failed their intended assertion. They are deliberate failures
outside the checkout, not disabled production tests or claims of exhaustive tests.

## Run and recorded validation

On macOS, with the pinned dependencies available, run from the repository root:

```sh
cargo test -p lean-host-mcp --test stage_runtime_contract -- --test-threads=1
cargo clippy -p lean-host-mcp --test stage_runtime_contract -- -D warnings
```

The tests need normal macOS process-inspection permission and `python3`. They do
not load Lean, build Mathlib, use the live server or signal unrelated processes.
Other platforms skip this native test entry; the portable kernel tests remain
separately runnable. A platform skip is not native validation there.

Initial exact-source validation on 2026-11-17: 34 checks passed in 1.42 seconds,
comprising 25 kernel tests, six native cases and three helper entry points. The
exact canonical source also passed the host workspace lint rules through an
isolated harness. The two routing mutations above failed as expected.

Registry-only host-package validation encountered an existing unavailable pin:
`lean-rs-worker-parent 0.9.10`. Both offline resolution and a normal registry
refresh rejected it. No canonical manifest or lockfile changed. Host-package
validation then passed in a disposable snapshot with local overrides for the
whole coupled SDK/semantic dependency graph: SDK commit `ccdaa4c` and
semantic-search commit `1ac78f7`. All 34 checks passed in 1.44 seconds after a
34.68-second parent-package build. Focused host Clippy passed with warnings denied;
Rust formatting, Markdown checks and the parent test binary's no-Lean-link check
also passed. That is distinct from registry-only reproducibility.

## What remains outside this test

Startup failures fail setup and trigger bounded cleanup; the tests do not qualify
a general recoverable factory API. They also do not cover per-request closure in
a live warm worker, detached descendants, frozen guardians, service death,
all-owner loss, exclusive writer custody, durable intent, SQLite, restart recovery
or production routing.

The next boundary is durable acceptance before native creation and GO: retain the
immutable attempt/stage-to-scope binding and consumed launch identity, then
reconcile the original work without repeating it after a lost reply or restart.
A database commit and process creation are not one atomic action. This prototype
supplies no durable protocol for that boundary. The
[service design](../../docs/native-execution-lifecycle-design.md) tracks those
separate obligations.
