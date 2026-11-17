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
| `stage_store_contract/cases.rs`, `cuts.rs` | Contract tests and disposable process-crash pause points |
| `stage_store_runtime_contract.rs` | Two macOS cases using the existing process fixture and runtime by relative path |

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

Observation authentication and saving a caller-owned result before ACK remain
external requirements. The journal here saves requests, not durable result assets.

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

The macOS target keeps the **same live runtime and actual child owner** while
reopening database handles. A dropped GO starts no target; a written GO with a
lost reply starts exactly one. SQL-to-kernel identity checks reject mixed records
before either launch gate changes. Results remain separate from resource closure.
An exclusive-lock contender blocked before cleanup checks for no protected group
and no later target writes at first acquisition.

Three deliberately broken copies were tested outside the checkout: reissuing a
consumed permit, redirecting an old ACK to a newer row, and omitting the SQL/native
identity check. Each compiled and failed its intended assertion. The last actually
started a target using the other row's permission. Generated logs, databases and
copied broken sources are not required to run or maintain the committed tests.

Recorded host-package validation, 2026-11-17: the database target passed 19 tests
and the native target passed 35 checks, including two actual process cases, 25
kernel tests, five connection tests and three helper entry points. The targets
reuse some checks; these are not 54 distinct native fault cases. Focused lint and
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
