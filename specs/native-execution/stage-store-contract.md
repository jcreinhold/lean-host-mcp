# Saved task identity and launch tests

Status: **test-only reference component, not part of the running server**. These
tests extend the [stage kernel](stage-kernel.md) and
[one-shot process tests](stage-runtime-contract.md). They do not complete service
restart recovery or the proposed execution-service redesign.

## Placement and dependency

The host package owns the tests because it will own native request history. The
SDK still owns child processes and the guardian; no SQLite API moved into it.

| File under `crates/lean-host-mcp/tests/` | Purpose |
| --- | --- |
| `stage_store_contract.rs` | Database-only test entry on Unix |
| `stage_store_contract/connection.rs` | Checked SQLite build, format-before-configuration and connection settings |
| `stage_store_contract/store.rs` | Separate caller journal and native store |
| `stage_store_contract/result_client.rs` | Save the caller copy before acknowledging its original native result |
| `stage_store_contract/caller_results.rs` | Result identity, two-commit crash cuts and SQL rejection tests |
| `stage_store_contract/cases.rs`, `cuts.rs` | Contract tests and disposable process-crash pause points |
| `stage_store_runtime_contract.rs` | Four macOS cases using the existing process fixture and runtime by relative path |

The code is private test support. It adds no public library API, production module,
actor framework or new crate. A Unix-only dev dependency pins `rusqlite 0.40.2`
with bundled SQLite; the lockfile selects `libsqlite3-sys 0.38.2`. Production builds
do not gain a SQLite dependency. Integrating a production store remains a separate
review, rather than hiding experimental code behind an unused public interface.

## What is saved

`CallerJournal` and `NativeStore` own **different database files**. The caller first
commits the original job/stage/explicit revision, exact intent, native ID and
mutation secret. An equal retry returns that original binding; changed intent or
reusing its native ID for a different caller operation fails. A new execution
revision is explicit, not an automatic response to a timeout.

The native store keeps immutable intent and scope binding. Each ID includes a
store UUID, increasing acceptance-window number and nonce. Missing IDs from closed
windows cannot be created; existing history remains readable. Opening an absent or
incomplete store never mints a new store identity.

Only a newly committed unused-to-consumed transition returns a private launch
permit. The runtime checks its original binding before GO. A lost reply or reopen
cannot produce another permit. A consumed record without a spawn observation is
uncertain; it is not evidence that nothing ran. Cancel and stopping/closed reports
seal unused permission. A database commit and process creation remain separate.

Launch state, observed spawn, stop requests, resource reports and result retention
are separate fields. Old callbacks target their own scope generation and owner.
Result expiry retains its ID and SHA-256 digest, preventing restoration or
replacement by a late callback. Resource reports never prove physical cleanup or
authorize conflicting work. The native owner must supply actual observations.

### Caller copy before acknowledgement

The caller schema is now **version 2**; the native schema remains **version 1**.
Existing caller-v1 files fail validation. These fresh test stores implement no
migration or compatibility fallback.

The caller's separate `accepted_results` table stores one immutable bounded inline
result, at most 64 KiB, for the original job/stage/revision. Its foreign key points
to the saved request. Reads recover the native identity from that request and
check the stored bytes against their SHA-256 digest. Equal retries return the
original copy; changed request, scope, result ID or contents fail. The code never
replaces a saved result.

`result_client::accept_then_ack` checks the complete original request and native
snapshot, including scope generation and owner. It then:

1. Commits the verified caller copy, or recovers an identical copy already saved.
2. Acknowledges (ACKs) that original native result in a separate transaction.

No transaction spans both databases. A lost caller-save reply leaves either no
copy or the original committed copy. A failed or lost ACK leaves the caller copy
readable through `saved_result`, even if the native store is unavailable. Native
expiry cannot delete that copy. Expired native metadata can reconcile a matching
caller copy; it cannot supply bytes that the caller never saved.

This enforces the tested client's order, not a native server's ability to prove
that a remote caller saved data. Raw native ACK still relies on that caller duty.
Snapshots and hashes do not authenticate producers, prove process cleanup or
permit launch replay. External result files remain outside this component.

## SQLite and opening files

Every connection checks the actual linked SQLite version/source ID and relevant
compile options. The reviewed build reports **SQLite 3.53.2**, source ID
`2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24`.
It includes the fix in SQLite's
[WAL-reset advisory](https://sqlite.org/wal.html#the_wal_reset_bug).

An existing file must pass its concrete role, version, required-column and native
store-identity checks **before** the helper requests WAL or other profile changes.
Only explicit exclusive creation makes a new 0600 file. Validation uses bounded
metadata reads, not a scan of task history. Its internal callback is reviewed
query-only code; Rust's `&Connection` does not itself enforce read-only SQL.

The refusal regression starts from clean DELETE-mode wrong-role, unsupported and
incomplete databases for both stores, plus empty files. It checks unchanged journal
mode, schema and bytes. It reproduced the old DELETE-to-WAL change before the fix.
This is a narrow guarantee: SQLite can recover journals during reads and checkpoint
WAL on close. The helper does not promise arbitrary rejected files never change.

Each accepted connection explicitly selects the Unix VFS and reads back WAL,
FULL synchronization, foreign keys, macOS full-sync requested, and a one-second
busy timeout. Changed settings fail rather than silently weakening the profile.
The build/run checks distinguish static bundled SQLite from system libraries;
`LIBSQLITE3_SYS_USE_PKG_CONFIG` can override bundling, so its value matters.

`fullfsync=1` is a request, not physical durability evidence: SQLite's Unix VFS
can fall back from failed `F_FULLFSYNC` to `fsync`. A busy timeout limits lock
contention, not arbitrary hung OS I/O. See SQLite's
[PRAGMA documentation](https://sqlite.org/pragma.html).

## What the tests establish

The database target covers equal/changed retries, concurrent consumption, monotone
scope generations, closed acceptance windows, cancellation, result retention,
connection settings and file refusal. Sixteen real helper-kill cases pause before
or after COMMIT, before returning, for caller binding, native CREATE, launch
consumption and result writes. They use checkpointed and uncheckpointed history.
Fresh connections verify the original records; both stores cold-open after all
old connections close. These are process-crash tests, not OS/power-loss tests.

Eight further helper-kill cases cover both sides of caller-result COMMIT and native
ACK COMMIT, with both checkpoint baselines. A fresh reader checks each cut;
retries then preserve the original binding, result ID, hash and bytes through
native expiry and a cold reopen. The after-caller-commit cut specifically covers
the gap between the two commits. Before caller commit neither copy nor ACK exists;
after it the caller copy exists, whether or not ACK committed.

Actual SQL INSERT rejection in the caller database prevents native ACK. Actual
native ACK UPDATE rejection preserves the caller's committed copy. The latter
case expires native bytes, reopens the caller journal and retries using that same
copy. Fresh readers verify the original rows, and errors retain SQLite's actual
1811 code and trigger message. These are statement errors, not uncertain COMMIT
or physical disk failures.

The macOS target keeps the **same live runtime and actual child owner** while
reopening database handles. A dropped GO starts no target; a written GO with a
lost reply starts exactly one. SQL-to-kernel identity checks reject mixed records
before either launch gate changes. The actual-result case also saves and ACKs
the caller copy, expires native bytes, reopens the caller journal and recovers exactly the same bytes while the original
runtime and child owner remain alive. Results remain separate from resource
closure. An exclusive-lock contender blocked before cleanup checks for no protected group
and no later target writes at first acquisition.

Two more cases reject actual SQLite UPDATEs before GO and while saving an
observed result. A persistent trigger (a rule stored in SQLite) raises error 1811
for every stage UPDATE. The error route preserves that exact code/message and
requests native cleanup through a helper with no database argument. The native
closure check also has no database argument. After group/guardian closure and
writer acquisition, the attempted SQL closed-report still fails, and fresh
readers see the unchanged stage record. Cleanup therefore did not require saving
its status. Before GO, no target starts; after GO, exactly one starts and the
observed result 42 remains absent from durable result history. Neither case
replays work or changes the original caller binding.

This is injected SQL statement rejection, not disk-full, failed flush, failed or
uncertain COMMIT, or cancellation during blocked storage I/O. SQLite ABORT reverses
the failing statement; these methods made no earlier writes in their transactions,
and transaction Drop requests rollback. The trigger was committed separately and
remains active throughout cleanup.

Five deliberately broken copies were tested outside the checkout: reissuing a
consumed permit, redirecting an old ACK to a newer row, omitting the SQL/native
identity check, omitting the shared storage-error cleanup request, and ACKing
before committing the caller copy. Each compiled and failed its intended assertion. The identity error actually started
a target using the other row's permission. Omitting cleanup left both original
roots alive at the two-second closure deadline; panic cleanup happened afterward,
not as a passed observation. The blocked contender helpers also timed out in those
negative cases. The early-ACK control fails the fresh-native-row assertion under
caller INSERT rejection: native history became Acknowledged while no caller copy
committed. ACK itself retains native bytes; the failure is the ordering violation,
not a claim of immediate data loss. No safety expiry supplied a pass. Generated
logs, databases and copied broken sources are not required to run or maintain the committed tests.

Recorded host-package validation, 2026-11-17: the database target passed 26 tests
and the native target passed 37 checks, including four actual process cases, 25
kernel tests, five connection tests and three helper entry points. The targets
reuse some checks; these are not 63 distinct native fault cases. Focused lint and
format checks accompany the integration.

## Run and remaining limits

From the repository root:

```sh
LIBSQLITE3_SYS_USE_PKG_CONFIG=0 cargo test -p lean-host-mcp \
  --test stage_store_contract --test stage_store_runtime_contract -- --test-threads=1
LIBSQLITE3_SYS_USE_PKG_CONFIG=0 cargo clippy -p lean-host-mcp \
  --test stage_store_contract --test stage_store_runtime_contract -- -D warnings
```

The native target needs normal macOS process-inspection permission and `python3`.
It skips elsewhere; only macOS native behavior was measured. No command loads Lean,
builds Mathlib, touches a live database or restarts the server.

The existing registry SDK `0.9.10` pin was unavailable during validation. Actual
host-package checks used a disposable snapshot with local SDK `ccdaa4c` and
semantic-search `1ac78f7` overrides. The committed manifest/lockfile contain no
local patches. Passing that build does not establish registry-only reproducibility.

Unimplemented or untested: service death/restart and reattachment, warm-worker
request closure, damaged-database recovery, pruning, backup restore, authenticated
observations, external result assets, real disk/sync errors, blocked-I/O
cancellation, OS crashes and power loss. See the
[execution-service design](../../docs/native-execution-lifecycle-design.md) for
those separate requirements. No server deployment or release follows from these
test-only changes.

## Next service-death boundary: proposal only

Stop adding storage features here. The next proposed test kills the disposable
**service process**, then starts a passive observer. It tests no replay and safe
cleanup; it does not recover the old process handles or resume work.

Use the already tested SDK topology: service → worker → healthy guardian. Only
the guardian retains the acknowledged shared artifact lock. The outer controller
and new observer inherit no protective lock, runtime, worker `Child` or guardian
control channel. Existing SDK tests cover service death with a healthy guardian;
this host-store combination has **not** run yet.

Test two cuts: after the unused-to-consumed commit but before GO, then after GO with
its reply and durable spawn record absent. For the latter cut, a private fixture
marker must witness an actual target start before killing the service; writing GO
alone does not ensure the target starts before cleanup. That marker is test
evidence, not a saved success or resource-closure fact. The new observer reopens
only the original caller/native IDs and must find `AlreadyConsumed`, never new
permission. Target-start counts must be zero at the before-GO cut and remain one
at the witnessed-start cut. A deliberately broken replay branch must make the
count exceed its bound.

An independent exclusive-lock contender must demonstrate it is blocked before
the fault, then inspect the protected group and worker immediately after its first
successful acquisition, and check for later payload writes. These observations
are not an atomic snapshot at acquisition. Require guardian exit and cleanup
before the safety deadline;
emergency rescue is a failed observation. Keep the SDK's owner-record check:
lock acquisition alone can succeed after guardian death while payloads still run.

A restarted observer cannot recover actual-child authority from a PID or a
SQLite row. The SDK offers no reopen/adopt-by-record lease. Thus this test must
not inject a generation-closed event into a new runtime or signal old payloads.
Missing durable spawn/result facts remain unknown even after separately observed
resource cleanup. Frozen guardian, missing custody records, simultaneous owner
loss and general service reattachment require a separate contract decision.
