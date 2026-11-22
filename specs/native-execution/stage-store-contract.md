# Saved task identity and launch tests

Status: the host can opt in to durable admission, lookup and cancellation. Dispatch and production service recovery remain disabled.
The contract tests also exercise the [stage kernel](stage-kernel.md) and [one-shot process fixture](stage-runtime-contract.md).

## Placement and dependency

The internal store and checked SQLite connection live under `crates/lean-host-mcp/src/native_store/`. The
`stage_store_contract` and `stage_store_runtime_contract` tests use that source. The SDK owns native processes and the
guardian. The non-default Unix `managed-native-store` feature enables the pinned bundled SQLite runtime. Ordinary
builds have the same runtime dependencies and tool table.

## Opt-in admission

On a disposable server built with `--features managed-native-store`, pass
`--managed-native-store /absolute/private-directory/stages.sqlite3`. The directory must already exist with mode
`0700` and belong to the server user. This option creates the database exclusively if absent or reopens the exact
database if present. It adds `lean_managed_admission` to the existing MCP tool table.

The tool takes a `request` string containing version 1 JSON with a `command` object. Use
`{"version":1,"command":{"op":"discover"}}` to obtain the store UUID and acceptance epoch. Before `create`, retain those
values with a fresh 16-byte nonce, stage number, randomly generated 32-byte capability, and the original intent bytes.
Send the store UUID, epoch, nonce and stage as `key`, and the bytes as `capability` and `intent`. Use `get` with the
same key and capability after an interrupted response. The same `create` returns the original key and intent.
A different intent or capability for that key is refused. The store-only route advertises
`dispatch_enabled: false`. A matched server with both an explicitly selected managed store and a prepared
generation advertises managed inspection on macOS and Linux.
Use `cancel` with the original key and capability. Its transaction seals an unused launch gate and records the stop
request together. After a lost cancel reply, either `get` or an equal `cancel` returns `execution: "not_started"` for
that same unused revision. A consumed gate reports `execution: "unknown"` even after cancellation. The opt-in route
accepts `discover`, `create`, `get`, `cancel` and `ack`. A saved result carries its original ID, SHA-256 digest and retained body.
After saving the bound result durably, send `ack` with the original key and capability plus `result_id` and
`result_digest`. ACK retains the result body while custody is unresolved. When ACK and confirmed closure are both
recorded, the same transaction removes the request and result bodies. It keeps the request SHA-256, result ID and
result digest. Compacted replies return `intent: null` and `intent_sha256`. An identical CREATE still returns the original
status, a changed CREATE fails by digest, and repeated START or ACK cannot revive the attempt. Unresolved stages keep
their requests for authenticated recovery. Results stay bounded and inline in SQLite, with no external result assets.
Unacknowledged results remain retrievable. The current inline policy prunes on confirmed closure or ACK, whichever is
last. It keeps at most 1,024 stage identities and rejects new intents at capacity while existing status, cancellation
and ACK stay available. At capacity, discovery can reclaim expired stages with durable ACK and confirmed closure.
The same transaction closes the acceptance epoch before removing any eligible current-epoch identity. Unresolved
custody and expired rows without positive ACK evidence stay recorded. A missing ID from a closed epoch cannot be
created again. Existing stores gain an additive ACK marker on checked open. Previously expired rows stay fenced
until the original caller supplies an authenticated repeat ACK.
The separate `custody` field reports the last trusted resource observation. Reading a result does not advance custody
or reissue a launch grant.

For the matched macOS prepared route, authenticated GET can return a recovery claim with the original revision
and worker generation. Send the complete claim and original key/capability with `drain`. Set `submit: false`
to observe the slot. Offering handoff with `submit: true` also requires the independent `authorization` secret
committed as `drain_authorization` at CREATE, plus a separate nonzero 32-byte `claimant` secret the claimant
persists before submitting. GET and slot observation never disclose either secret. The transaction binds the claim
to the original attempt, revision, and exact generation. It seals an unused gate and preserves a spent gate. An equal
retry recovers the claim, while a rival receives `recovery_claim_busy`. The original guardian retains cleanup authority.
An offer or acceptance
does not certify closure. The original guardian decides eligibility and retains the original hard deadline
when armed. Select the SDK drain-slot mode with `LEAN_RS_WORKER_GUARDIAN_DRAIN_SLOT=1` for the matched
guardian. The replacement records closure only from the exact SDK witness or independently observed whole-group
absence when the original generation directory has already been removed.

## What is saved

`CallerJournal` and `NativeStore` own **different database files**. The caller first commits the original
job/stage/explicit revision, exact intent, native ID and mutation secret. An equal retry returns that original binding;
changed intent or reusing its native ID for a different caller operation fails. A new execution revision is explicit,
not an automatic response to a timeout.

The native store keeps immutable intent and scope binding. Each ID includes a store UUID, increasing acceptance-window
number and nonce. Missing IDs from closed windows cannot be created; existing history remains readable. Opening an
absent or incomplete store never mints a new store identity.

Only a newly committed unused-to-consumed transition returns a private launch permit. The runtime checks its original
binding before GO. A lost reply or reopen cannot produce another permit. A consumed record without a spawn observation
is uncertain; it is not evidence that nothing ran. Cancel and stopping/closed reports seal unused permission. A database
commit and process creation remain separate.

Launch state, observed spawn, stop requests, resource reports and result retention are separate fields. Old callbacks
target their own scope generation and owner. Result expiry retains its ID and SHA-256 digest, preventing restoration or
replacement by a late callback. Resource reports never prove physical cleanup or authorize conflicting work. The native
owner must supply actual observations.

### Caller copy before acknowledgement

The caller schema is now **version 2**; the native schema remains **version 1**. Existing caller-v1 files fail
validation. These fresh test stores implement no migration or compatibility fallback.

The caller's separate `accepted_results` table stores one immutable bounded inline result, at most 64 KiB, for the
original job/stage/revision. Its foreign key points to the saved request. Reads recover the native identity from that
request and check the stored bytes against their SHA-256 digest. Equal retries return the original copy; changed
request, scope, result ID or contents fail. The code never replaces a saved result.

`result_client::accept_then_ack` checks the complete original request and native snapshot, including scope generation
and owner. It then:

1. Commits the verified caller copy, or recovers an identical copy already saved.
2. Acknowledges (ACKs) that original native result in a separate transaction.

No transaction spans both databases. A lost caller-save reply leaves either no copy or the original committed copy. A
failed or lost ACK leaves the caller copy readable through `saved_result`, even if the native store is unavailable.
Native expiry cannot delete that copy. Expired native metadata can reconcile a matching caller copy; it cannot supply
bytes that the caller never saved.

This enforces the tested client's order, not a native server's ability to prove that a remote caller saved data. Raw
native ACK still relies on that caller duty. Snapshots and hashes do not authenticate producers, prove process cleanup
or permit launch replay. External result files remain outside this component.

## Private client boundary

`store/boundary.rs` decodes bounded, versioned client requests. Only its decoder creates checked `ClientRequest` values;
that type does not implement Deserialize. The client operations are CREATE, GET, CANCEL and ACK, plus discovery and an
explicitly disabled START. Scope binding, launch consumption and runtime observations have no client wire variants. This
adds no listener or production API and does not authenticate native producers.

Client GET, CANCEL and ACK check the original capability. An existing CREATE authenticates before comparing saved
intent. ACK checks result ID and digest in its mutation transaction. Cancellation before admission seals unused
permission without inventing a scope or closure. A later scope binding fails without changing scope heads, including
after reopen.

Store reads check borrowed SQLite BLOB types and lengths before making Rust-owned copies. Fixed identities and digests
use fixed arrays. Native reads reject invalid scope/null combinations, out-of-range enums/booleans and mismatched result
hashes. These checks bound Rust-owned row copies, not SQLite's internal buffers. A generic retained fact can exist
without a target-spawn witness; it does not by itself establish a semantic result or resource closure.

Nine new checks cover this boundary. Five additional broken scratch copies compiled and failed their intended checks:
omitted GET authentication, sealed-bind rejection, ACK digest validation, BLOB bounds, or a committed scope-head change
during a rejected read. A pre-fix regression also showed that a Serde internally tagged unit variant ignored discovery
payload fields; strict `Discover {}` rejects them. Capabilities and decoder diagnostics are redacted.

## SQLite and opening files

Every connection checks the actual linked SQLite version/source ID and relevant compile options. The pinned build
requires **SQLite 3.54.1**, source ID
`2026-10-16 14:45:20 555e07ef33f69a0957d24d8e555b702464a131d6f1607f4c3c1e4551e8e6ca76`. It includes the fix in SQLite's
[WAL-reset advisory](https://sqlite.org/wal.html#the_wal_reset_bug).

An existing file must pass its concrete role, version, required-column and native store-identity checks **before** the
helper requests WAL or other profile changes. Only explicit exclusive creation makes a new 0600 file. Validation uses
bounded metadata reads, not a scan of task history. Its internal callback is reviewed query-only code; Rust's
`&Connection` does not itself enforce read-only SQL.

The native store also checks SQLite structural integrity with `quick_check(1)` before applying its write profile.
If the store cannot be opened, stop the managed service and preserve the database and its sidecars together for
offline quarantine. Keep the SDK ownership registry in place. Selecting a fresh private path is an operator action
after independently reconciling native custody. It mints a new store UUID and old IDs cannot authorize a launch
there. The managed route does not import damaged history or automate restore.

The refusal regression starts from clean DELETE-mode wrong-role, unsupported and incomplete databases for both stores,
plus empty files. It checks unchanged journal mode, schema and bytes. It reproduced the old DELETE-to-WAL change before
the fix. This is a narrow guarantee: SQLite can recover journals during reads and checkpoint WAL on close. The helper
does not promise arbitrary rejected files never change.

Each accepted connection explicitly selects the Unix VFS and reads back WAL, FULL synchronization, foreign keys, macOS
full-sync requested, and a one-second busy timeout. Changed settings fail rather than silently weakening the profile.
The build/run checks distinguish static bundled SQLite from system libraries; `LIBSQLITE3_SYS_USE_PKG_CONFIG` can
override bundling, so its value matters.

`fullfsync=1` is a request, not physical durability evidence: SQLite's Unix VFS can fall back from failed `F_FULLFSYNC`
to `fsync`. A busy timeout limits lock contention, not arbitrary hung OS I/O. See SQLite's
[PRAGMA documentation](https://sqlite.org/pragma.html).

## O3 source acceptance

Host commit `858648b` used registry-only lean-rs `0.10.10` (signed SDK source
`c15e80d45efd65c1e6a4ece11e85b67ea0292aac`) on macOS with Lean 4.36. The existing
`managed_inspection.py` fixture verified the cold result, recovery of the same reply after restart, authenticated
stopped-broker closure after two collectors, and unrelated work during unresolved custody. The original broker's
resume and a repeated START did not replay its consumed grant. Exact ACK removed the closed attempt's request
and result bodies. The ordinary tool table and the installed Init refusal for the same declaration probe matched
the published 0.10.9 baseline.

- **Linked runtime and open profile.** The checked connection reads SQLite's linked version and source ID,
  checks compile options, explicitly selects the `unix` VFS, and reads back WAL, FULL (`2`), `fullfsync=1`,
  foreign keys enabled, and a 1,000 ms busy timeout. The reopen, wrong-runtime and changed-profile tests cover
  these checks. A VFS selected by name and a requested full-sync setting are distinct from a measured disk flush.
- **Refused writes.** Persistent trigger cuts exercised caller INSERT refusal, native ACK UPDATE refusal,
  and native updates before GO and during result publication. The opt-in MCP test verified the caller's failed
  save left the result retained. A managed declaration cut also verified a rejected result write left the result
  absent while verified custody closure could still commit. A deferred foreign-key fault let the GO UPDATE
  succeed and then made SQLite refuse COMMIT with `SQLITE_CONSTRAINT_FOREIGNKEY`. No grant escaped, and
  a fresh connection saw the unchanged pending stage. With the fault removed, discarding the successful
  grant and reopening returned AlreadyConsumed and preserved absence of spawn and result evidence.
- **Bounded full store.** The admission cap test refused new stage identities and preserved old lookup and
  cancellation. A separate test capped the disposable SQLite connection at its existing page count. It
  returned `SQLITE_FULL` on a large CREATE, admitted no row, and could still read and cancel the original
  stage on the capped connection. Reopen confirmed that cancellation sealed launch.
- **Corruption and controlled replacement.** The actual broker refused a damaged SQLite file while a separate
  worker and guardian were alive. The owner record was unchanged and the exact read-lock paths still refused
  an exclusive lock. The separate offline quarantine test kept the damaged bytes and opened an explicit fresh
  path with a new UUID. The new store rejected the old ID. This exercise did not restore damaged history.
- **Retention and assets.** Results are bounded inline SQLite bytes with an ID and SHA-256. External result-asset
  publication, unavailable external assets, and external asset GC do not apply to this route. Inline digest checks,
  save-before-ACK, expiry, and closed-epoch pruning were exercised independently of custody.

| Fault category | Evidence and limits |
| --- | --- |
| Process crash | Real process kills around SQLite commits, and disposable broker loss with real custody, exercised restart behavior without replay. The pre/post-COMMIT cuts test what survives process loss, rather than a failing COMMIT syscall. |
| OS crash | Unrun. No kernel crash, VM power cut or filesystem recovery assertion is qualified here. |
| Physical power loss | Unrun. No evidence establishes physical flush completion or power-loss persistence. |

The bounded page limit exercises SQLite capacity without filling a filesystem. The deferred constraint denies
COMMIT without injecting a disk or flush failure. Process death after a committed write qualifies a lost grant
reply, while a storage API that reports an ambiguous COMMIT error remains unqualified. H0 owns the independently counted target-start and actor/registry capacity checks.
This is source acceptance, with no managed host publication or live activation.

## What the tests establish

The database target covers equal/changed retries, concurrent consumption, monotone scope generations, closed acceptance
windows, cancellation, result retention, connection settings and file refusal. Sixteen real helper-kill cases pause
before or after COMMIT, before returning, for caller binding, native CREATE, launch consumption and result writes. They
use checkpointed and uncheckpointed history. Fresh connections verify the original records; both stores cold-open after
all old connections close. These are process-crash tests, not OS/power-loss tests.

Eight further helper-kill cases cover both sides of caller-result COMMIT and native ACK COMMIT, with both checkpoint
baselines. A fresh reader checks each cut; retries then preserve the original binding, result ID, hash and bytes through
native expiry and a cold reopen. The after-caller-commit cut specifically covers the gap between the two commits. Before
caller commit neither copy nor ACK exists; after it the caller copy exists, whether or not ACK committed.

Actual SQL INSERT rejection in the caller database prevents native ACK. Actual native ACK UPDATE rejection preserves the
caller's committed copy. The latter case expires native bytes, reopens the caller journal and retries using that same
copy. Fresh readers verify the original rows, and errors retain SQLite's actual 1811 code and trigger message. These are
statement errors, not uncertain COMMIT or physical disk failures.

The macOS target keeps the **same live runtime and actual child owner** while reopening database handles. A dropped GO
starts no target; a written GO with a lost reply starts exactly one. SQL-to-kernel identity checks reject mixed records
before either launch gate changes. The actual-result case also saves and ACKs the caller copy, expires native bytes,
reopens the caller journal and recovers exactly the same bytes while the original runtime and child owner remain alive.
Results remain separate from resource closure. An exclusive-lock contender blocked before cleanup checks for no
protected group and no later target writes at first acquisition.

Two more cases reject actual SQLite UPDATEs before GO and while saving an observed result. A persistent trigger (a rule
stored in SQLite) raises error 1811 for every stage UPDATE. The error route preserves that exact code/message and
requests native cleanup through a helper with no database argument. The native closure check also has no database
argument. After group/guardian closure and writer acquisition, the attempted SQL closed-report still fails, and fresh
readers see the unchanged stage record. Cleanup therefore did not require saving its status. Before GO, no target
starts; after GO, exactly one starts and the observed result 42 remains absent from durable result history. Neither case
replays work or changes the original caller binding.

This is injected SQL statement rejection, not disk-full, failed flush, failed or uncertain COMMIT, or cancellation
during blocked storage I/O. SQLite ABORT reverses the failing statement; these methods made no earlier writes in their
transactions, and transaction Drop requests rollback. The trigger was committed separately and remains active throughout
cleanup.

Six deliberately broken copies were tested outside the checkout: reissuing a consumed permit, redirecting an old ACK to
a newer row, omitting the SQL/native identity check, omitting the shared storage-error cleanup request, and ACKing
before committing the caller copy, and replaying a consumed task in a fresh observer. Each compiled and failed its
intended assertion. The identity error actually started a target using the other row's permission. Omitting cleanup left
both original roots alive at the two-second closure deadline; panic cleanup happened afterward, not as a passed
observation. The blocked contender helpers also timed out in those negative cases. The early-ACK control fails the
fresh-native-row assertion under caller INSERT rejection: native history became Acknowledged while no caller copy
committed. ACK itself retains native bytes; the failure is the ordering violation, not a claim of immediate data loss.
No safety expiry supplied a pass. Generated logs, databases and copied broken sources are not required to run or
maintain the committed tests.

Recorded host-package validation, 2026-11-17: the database target passed **35 checks** and the native target passed **41
checks**, including six actual process cases, 25 kernel tests, five connection tests and five helper entry points. The
targets reuse checks; their sum is not 76 distinct native fault cases. Focused Clippy with warnings denied and
repository formatting checks passed. An independent review checked the source, API, failure checks and qualification
records.

## Run and remaining limits

From the repository root:

```sh
LIBSQLITE3_SYS_USE_PKG_CONFIG=0 cargo test -p lean-host-mcp \
  --test stage_store_contract --test stage_store_runtime_contract -- --test-threads=1
LIBSQLITE3_SYS_USE_PKG_CONFIG=0 cargo clippy -p lean-host-mcp \
  --test stage_store_contract --test stage_store_runtime_contract -- -D warnings
```

The native target needs macOS process/socket setup permission and `python3`. It skips elsewhere; only macOS native
behavior was measured. No test invokes the Lean elaborator/kernel, builds Lean or Mathlib, touches a live database or
restarts the server.

Actual host-package qualification used a disposable snapshot matching these tracked revisions:

| Input | Qualified revision |
| --- | --- |
| Host, including the separate identical-CREATE retry assertion | `d50424ff4f02c0ee427692425b0ff941f6d706f4` |
| Local SDK snapshot | `4ae59cd0e69ceccd8aae1f24532df107fdbcd409` |
| Local semantic-search source | `1ac78f72f2647d74b722eeada53adc8bc5df44a7` |

Only the disposable lockfile resolved the 12 local path overrides. SDK build inputs were checked against the pin;
canonical manifests and lockfile stayed unchanged. The SDK snapshot had no built Lean fixture and recorded a zero
fixture digest. Neither test executable links `libleanshared`; no Lean fixture or Mathlib build ran.

The first tool-sandbox run failed six native scope-preparation cases with EPERM, before target roots spawned. One
approved retry outside that sandbox passed all 41 native checks. This records the conditions of this qualification, not
a change to server security or a production recovery mechanism.

The registry-only SDK `0.9.10` pin remained unavailable. These results qualify the local override composition; they do
not establish registry-only reproducibility.

That earlier local-override run did not qualify replacement-service execution and reattachment, warm-worker request
closure, damaged-database recovery, pruning, backup restore, authenticated observations, external result assets,
real disk/sync errors, blocked-I/O cancellation, OS crashes or power loss. The registry-backed O3 evidence above
supersedes that run where it covers the same behavior. See the
[execution-service design](https://github.com/jcreinhold/proofs/blob/main/docs/projects/lean-tooling/design/native-execution-lifecycle.md) for those separate requirements. No server
deployment or release follows from these test-only changes.

## Service death with a healthy guardian

Two macOS cases now SIGKILL the actual disposable service child. Only that service owns the runtime, actual worker
`Child`, GO sender and private guardian channel. The worker's actual child guardian holds the only acknowledged shared
artifact lock; the worker drops its own copy and the target closes inherited descriptors. The outer controller and fresh
observer retain no old protective lock, worker handle or control channel. Birth and parent-PID markers check this
topology; they grant no right to signal a saved PID.

The cuts follow (1) the unused-to-consumed commit, with the GO pipe still open, and (2) a written GO with actual target
start and heartbeat witnessed, while the service saves no spawn or result observation. The actual service `Child`
receives SIGKILL and its retained parent checks that exit signal. No service destructor can perform cleanup. Before GO,
service death also closes the pipe: EOF can stop that waiting worker. The witnessed-start case tests cleanup of a
running target.

Before either fault, a separately owned contender proves its first real exclusive-lock attempt blocks. After acquisition
it inspects the original worker and group, checks the SDK's owner-record fence against the held lock's device and inode,
then marks the payload log and checks for later writes. These are successive observations, not an atomic snapshot at
acquisition. Both cases observed absent worker/group, managed-writer permission and zero later payload bytes. They also
require successful guardian return and exact guardian death, separately from EX.

After cleanup and release of the contender's EX, a new observer process opens only the original caller/native records.
It gets `AlreadyConsumed`, creates no runtime or worker and leaves the row unchanged: consumed launch, unknown
spawn/result, resource report still Open. It emits no generation-closed event. The before-GO case has zero target
starts; the witnessed-start case retains exactly one. This observer does not run a replacement execution service or
recover its old handles.

The external replay control deliberately ignores `AlreadyConsumed` and creates a fresh runtime for the same original
logical task. It starts a second real root and target, then cleans up through that new actual owner. The common
start-count audit fails with two distinct target PIDs for the same stage. This tests the replay oracle against a wrong
policy, not an existing production restart implementation.

All passing cases finish before three seconds. Root and target eight-second fallbacks remain; the service-death guardian
also has an eight-second self-kill watchdog. Any fallback invalidates the run. The guardian watchdog can release its
lock before payload expiry, so it is failed-test rescue, never a recovery policy. Markers are best-effort before
unconditional self-kill; failed marker writes do not suppress rescue. Timers do not bound frozen processes or hung OS
operations.

The SDK's existing guardian-loss counterexample remains separate: bare EX can succeed while payload writes continue, and
its managed owner-record check refuses work. This slice calls that check but does not rerun the SDK counterexample or
reconstruct guardian signal authority from its saved identity.

## Next contract decision

Storage work stops here. These tests cover service death **with a healthy guardian**, intact owner records and
descendants in the qualified private group. They do not establish progress after a frozen last custodian or simultaneous
service/guardian loss. The SDK offers no reopen/adopt-by-record lease.

Before wiring a replacement service, choose its supported fault scope. Keep the healthy-guardian limit, or separately
qualify an independent authority that can stop old payloads and resolve custody when that guardian cannot act. A SQLite
row, PID, result expiry or elapsed deadline supplies no such authority. Losing all custodians may release the file lock
while payloads remain alive; retain the owner-record fence. Do not turn an unresolved scope into a closed one to free
capacity. No stronger recovery contract or new authority is implemented here.
