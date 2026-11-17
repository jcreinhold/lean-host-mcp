# Native execution lifecycle and self-healing

Status, 2026-11-17: **proposed replacement with bounded private tests; no production service or deployment
qualification.** This contract retains the useful requirements from the superseded lifecycle-evidence draft. Git
preserves that draft's abandoned storage alternatives and historical inspection state.

Use this document for identity, execution, custody, recovery and storage requirements. The
[cutover plan](native-execution-cutover-plan.md) owns the concrete launcher, deletion, installation-fence and rollback
inventory. [Model and prototype contracts](../specs/native-execution/README.md) own replay instructions and detailed
test scope. Documents record evidence and obligations; they do not authorize deployment, pushes or proof-input changes.

- [Scope](#1-decision-and-scope), [evidence](#2-verified-baseline-and-concrete-failures) and
  [owners](#3-current-owners-and-exact-paths)
- [Process boundary](#4-proposed-process-architecture), [storage](#5-sqlite-decision-paths-and-schema) and
  [protocol](#6-protocol-and-state-transitions)
- [Deadlines](#7-deadlines-inactivity-and-defaults) and [recovery](#8-self-healing-and-pragmatic-resource-recovery)
- [Acceptance](#11-acceptance-and-fault-matrix), [validation](#13-validation-and-refinement),
  [model status](#14-model-evidence) and [storage qualification](#15-storage-qualification)
- [Ordered remaining gates](#16-remaining-implementation-and-acceptance-gates)

## 1. Decision and scope

One native Rust execution service owns admission policy, immutable attempt identity, deadlines, status and persistence.
The SDK owns actual process/resource custody. The host remains the scheduling client; proof repositories retain source
snapshots, theorem scope and acceptance gates. The service must work without IUT names, layouts or Python registries.

Reuse Lean project actors, workers, sessions, artifact locks and qualified process-group machinery. The preferred
recovery boundary is **one SDK operation over the existing native owner registry**, not another custody database,
manifest tree or permanent recovery daemon. Mapping the proposed service onto the existing worker/guardian topology
still needs the containment and warm-integration gates below. Preserve typed `ProjectMessage` dispatch; this work does
not require an actor-interface rewrite.

The client supplies an operation and receives a durable handle/result. Guardian ACKs, descriptor transfer, heartbeats,
storage transactions and process-group observation stay below that boundary. Native policy must replace old grant and
expiry authorities rather than add another queue. The design criteria are separation of intent, scheduling, custody,
observation and recovery, and fewer caller obligations; adding actors or SQLite alone does not establish simplicity.

Success means abandoned work recovers without routine manual drains or a permanent fence caused only by missing history.
An interrupted outcome may remain unknown. New explicitly authorized work can proceed after current resources are
established available; the lost result never authorizes automatic payload replay. A topology with fewer owners is
preferable only if its qualified containment boundary preserves exclusion through owner death.

## 2. Verified baseline and concrete failures

The current recovery review uses SDK `4ae59cd`; earlier inspection used host `64b5b86` and SDK `0976c61`. The table
distinguishes recorded evidence from replacement-service requirements. It is not a fresh installed-image qualification.

| Area | Evidence and limit |
| --- | --- |
| Identity, one-use dispatch, caller-save-before-ACK | Private kernel/store/runtime contracts exist; no production store, listener or RPC dispatch is wired. |
| Client authority and bounded reads | Host `ea996dd` adds strict private CREATE/GET/CANCEL/ACK decoding, disabled client START, capability checks, sealed pre-admission cancel and bounded Rust-owned row copies. It does not authenticate native producers or install a service. |
| Actual host-package checks | Reported at host `d50424f`: 35 database and 41 runtime checks plus focused Clippy, using disposable SDK `4ae59cd` and semantic `1ac78f7` path overrides. These overlapping checks are not 76 distinct native fault cases. Registry-only reproducibility, producer bootstrap and production dispatch remain unqualified. |
| Service death | Scoped tests kill the actual service while a healthy guardian survives; a later passive observer reads evidence. No replacement service adopts work or regains native authority. |
| Custodian metadata and compute capacity | SDK `120767d` retains a live guardian record after payload closure without occupying a compute slot. Its drain-barrier test is not SIGSTOP recovery. |
| Writer admission | SDK `4ae59cd` closes ungranted candidate EX and all partial locks before retrying, preserving demand, original deadline and inode checks. This removes a takeover prerequisite, not the recovery gap. |
| macOS process tokens | Disposable same-boot supplied-token tests cover exec-generation rejection, nonparent signaling and running-orphan termination with a prospective exit observer. Durable discovery, boot binding, late observation and complete-group containment are not established. |
| Frozen guardian and takeover | Still a qualification gate. Stopped-child controls and pre-worker EPERM failures do not establish actual frozen-guardian recovery. |
| Formal models | Port `a309cc6` replaces abstract Python explorers with TLA+; its owner records 155 required TLC cases with intended results. A larger control case remains incomplete. See §14 for the boundaries; no composition or runtime refinement is claimed. |

The actual-package qualification report and its first permission failures are retained in
`/private/tmp/lean-service-contract-20261117/spec-update-proposal.md`. The first sandboxed run failed six
owner-preparation cases before targets spawned; the permitted retry passed 41 runtime checks. The disposable lockfile
alone used the local overrides. Canonical manifests/lockfile and installed state were unchanged. Detailed private-store
scope belongs in the [store contract](../specs/native-execution/stage-store-contract.md), whose dated checkpoints must
be read as dated.

These failures explain the requirements that remain after consolidation:

| Recorded failure | Evidence | Required behavior |
| --- | --- | --- |
| Repository queue serialized native work: roughly 598s total, 570s waiting | Proofs `135adf6203`, `7b959123c2`; schema-3 archive of 4,876 native operations | No second repository queue or SQL capacity hold. |
| `/var` versus `/private/var`, FIFO metadata, detached formatter children, and physical-domain expansion | Host `714fc5e`, `65223a1`, `d1b7f56`; SDK `b55d935`, `328a9d6`; lean-fmt `d3aaa47`, `81899c2` | Canonical/inode-bound domains, bounded regular-file metadata, managed inherited groups and no approximate fallback. |
| Warm readers blocked foreign writers; partial multi-domain admission retained locks | SDK `c6067f2`, `0976c61`; host `2a46a5a`, `e1c8a0f`, `5fb7dd8` | Targeted retirement, full partial-set release and one original admission budget. |
| Strict/lint windows overlapped a roughly 302.5s BranchStar writer that failed compilation | `/tmp/proofs-07m7-current-admission-diagnosis.json` | Contention is not proof failure; report bounded admission and dispatch phases separately. |
| Lost first writer reply and ambiguous post-ACK reader failure | `/tmp/lean-writer-admission-identity-assessment.json`, `/tmp/proofs-07m7-225-native-assessment.json` | Durable original identity before helper spawn; separate preparation, launch permission, spawn, result and closure. |
| An alive idle writer controller can retain EX while waiting for input | Inspected `shared_writer.rs` controller loop; exact paths in the cutover inventory | Independently enforced idle-custody expiry. This was a code finding, not a reproduced incident. |

Prior finite mixed acceptance measured target 44.878s, consumer 45.749s and semantic check 36.965s, with overlapping
axiom payloads and no new swapouts. Evidence: `/tmp/proofs-v1-final-acceptance.json` and
`/tmp/proofs-v1-mixed-whole-union-corrected/independent-audit.json`. Those normal-workload observations do not qualify
lost controllers, frozen custodians or this replacement architecture. Historical proof outcomes and receipts remain
unchanged.

## 3. Current owners and exact paths

| Boundary | Existing code and responsibility |
| --- | --- |
| Semantic host | `crates/lean-host-mcp/src/{broker,project}.rs`: project selection, warm sessions, sequential Lean requests and cancellation. `tasks.rs`, `request_log.rs`, `request_journal.rs`: semantic tickets/results, with their existing IDs and ACK meaning. |
| Native process custody | SDK `crates/lean-toolchain/src/{native_guardian,process_group}.rs` and `native_guardian/ownership.rs`: real scope identity, descriptor holders, stop/wait and resource observation. |
| External command adapters | Host `shared_writer.rs`, `native_reader.rs` and their submodules; SDK `shared_writer.rs`, `shared_reader.rs`, `writer_demand.rs`: complete-domain admission and helper/process ownership. |
| Caller provenance and acceptance | The caller retains its original source/job/stage binding and final acceptance checks. It must shed helper lifetime, native capacity and expiry policy. |

The [cutover inventory](native-execution-cutover-plan.md#verified-ownership-and-deletion-targets) identifies the three
active external helper-owner families and all concrete callers. It also owns exact historical image/configuration
observations. Existing semantic journals remain separately owned evidence; the new service must not let arbitrary
helpers write their schema or reinterpret old tickets as native attempts.

## 4. Proposed process architecture

The diagram assigns responsibilities; it does not authorize another persistent supervisor beside the existing SDK. Final
mapping, especially writer custody, remains subject to qualification.

```text
proof client / semantic host
    | typed local requests; durable original attempt binding
    v
native execution service: admission, deadlines, status, SQLite writer
    | generation or one-shot scope
    v
native supervisor -------- private lifeline -------- custody guardian
    | actual children                             | independent enforcement
    v                                             | complete SH/EX domain union
protected inherited process group -----------------+
```

Proposed command: `lean-host-mcp execution-service`, independent of an MCP connection. A singleton, discovered short
local socket and per-user service installation are proposed; no installed service is established here. Linux service
management is separately qualified. The service never executes Lean FFI.

The service owns one bounded pending-admission policy, proposed capacity 64, and one global grant authority. Conflicting
requests use FIFO with finite waiting budgets; this is not global fairness over unmanaged processes. Bounded I/O workers
return observations without blocking the scheduling actor. Move broker grant decisions into that policy while retaining
project/profile selection and idle-retirement hints. Native slots represent physical occupancy, released only after
actual closure; they are not another independently configurable permit.

An external command has an attempt scope. A warm worker has one generation scope spanning sequential attempts, not a new
supervisor per request. Request timeout may require stopping the generation, but request completion alone does not
retire it. No unrelated active request shares that generation. Preserve session reuse and writer-demand retirement.

The supervisor validates commands, stages inputs, owns the actual child, enforces bounded output and requests stop. The
independently scheduled guardian retains custody and watches both service authority and supervisor liveness. It cannot
rely on a possibly frozen supervisor, busy Lean actor, database transaction or congested executor to enforce expiry.
Neither service authority nor supervisor liveness alone can renew the other. EOF triggers cleanup; a channel remaining
open without valid renewal expires. Cleanup continues if evidence publication fails.

Audit every original, transferred and inherited descriptor holder. Use validated transfers and ACKs before the sender
drops custody; set CLOEXEC except for deliberately selected descriptors. Shared open-file descriptions require
**close-only** cleanup: no explicit unlock or SH/EX mode conversion while live custody exists. Independent opens are not
assumed equivalent to alias transfer. A helper's own alias release, its actual reap and complete protected-group closure
are distinct observations; none alone certifies external borrowed aliases or surviving descendants.

Service-only, supervisor-only and guardian-only loss each need a stated surviving authority and closure path.
Simultaneous loss of every useful anchor, leader loss with descendants, fork/exec races and descriptor-closing payloads
are separate cases. The superseded dual-alias topology is not selected or qualified. Fewer processes do not fix an
owner-death lock-release window; any simpler direct owner needs the same independent contention/closure oracle.

The supported process contract is cooperating canonical-domain readers/writers and managed inherited groups. Deliberate
detach/regroup, lock-inode replacement, undeclared writes and arbitrary unmanaged processes remain outside it. macOS
leader pinning does not qualify Linux signaling after leader reap. A serialized PID/PGID, birth string, token rejection
or successful signal is not complete scope custody or closure.

## 5. SQLite decision, paths and schema

SQLite is the selected intent/history store: ordered acceptance, atomic claims/events, original-ID lookup and bounded
retention. It is not the process owner or a lock service. Reuse the SDK native owner registry for recovery facts; do not
add a second custody manifest tree or SQL recovery scheduler.

The proposed platform application-state directory contains `lifecycle.sqlite3`, service `owner.lock` and
`assets/<attempt-id>/`; `--execution-state-dir` would override it. On macOS the proposed default is
`~/Library/Application Support/lean-host-mcp/execution/`. Discovery reports endpoint, store UUID and protocol; use a
short runtime socket path. Private directories/files use 0700/0600, reject special-file/symlink substitutions and check
same-user local peers. This does not protect against hostile same-user processes.

Proposed schema version 1 separates:

- Store metadata and service generations: schema/migration versions, random store UUID, boot and process identity.
- Immutable attempts: acceptance epoch/nonce, complete intent bytes/digest, exact input/command/domain binding and
  budget.
- Attempt state and append-only deduplicated events: transactional observations and revision, not custody authority.
- Independent scopes and attempt/scope associations: generation, domains, supervisor/guardian identity, stage and finite
  request deadline. Sequential attempts can share one warm scope without adding capacity permits.
- Bounded results: inline bytes or immutable digest-bound assets with explicit publication state.

One service-owned connection writes short transactions; event insertion and state projection update atomically. Validate
store identity/schema and role before changing persistent settings. Configure and read back WAL, `synchronous=FULL` (2)
and foreign keys on each relevant fresh/reopened/pooled connection. Proposed busy budget: 1s, charged to the original
request deadline; it does not bound hung kernel I/O. No helper writes SQL and no UPSERT recreates missing/expired
attempts.

On qualified macOS SQLite/VFS builds, request/read back fullfsync ON. Unknown PRAGMAs may be ignored, and a requested
sync setting is not proof that a device honored it. NORMAL in WAL permits committed data loss after OS/power failure;
FULL adds commit synchronization; EXTRA adds no WAL durability beyond FULL. fullfsync and external-directory behavior
need platform qualification; checkpoint_fullfsync alone is not a substitute. See SQLite's
[synchronous](https://sqlite.org/pragma.html#pragma_synchronous),
[fullfsync](https://sqlite.org/pragma.html#pragma_fullfsync) and
[checkpoint_fullfsync](https://sqlite.org/pragma.html#pragma_checkpoint_fullfsync) contracts.

Required settings/commit/sync failure refuses a new durable admission or GO acknowledgment, without silent downgrade. A
failed/lost commit reply can be ambiguous: reconcile the original immutable ID, never assume rollback or replay its
payload. Storage initialization is controlled; avoid journal-mode transitions on the live operation path. Existing-scope
cleanup remains independent of successful SQL writes.

### Caller binding and acceptance epochs

Before the first CREATE RPC, the caller durably saves the unique key **(original job ID, stage key, explicit execution
revision)** with immutable values **(input digest, store UUID, epoch/nonce, mutation capability)**. The digest is a
value, not part of the unique key: changed input under the same key must fail. Retain byte-exact argv, environment and
Unix path encoding, explicit environment-inheritance semantics and complete input bytes or durable objects. A digest
alone cannot recover an intent. The client library hides persistence mechanics; clients without a durable original
operation key cannot claim crash-safe resumption.

Specify whether the saved environment is the full effective environment or overrides plus declared inherited inputs; do
not present an overrides-only snapshot as a complete effective environment.

The library persists a random mutation capability before CREATE; the native store saves its hash. Identical ID, intent
and capability retries recover the existing attempt. Conflicts fail. Lost CREATE/START replies cause GET/reconciliation
of that original binding. A fresh execution revision is explicit new work, never an automatic recovery from expiry.

Discovery advertises a persisted monotonic acceptance epoch. Proposed rotation is daily and on fresh-store creation;
wall time may trigger rotation but cannot reopen an epoch. Commit prior closure and the next sequence before advertising
it. Existing IDs remain readable and identical CREATE retries remain idempotent after closure; missing IDs from closed
epochs cannot create. Keep deduplication rows/tombstones throughout the open epoch, then prune within bounded retention.
A controlled restore/replacement allocates a new store UUID before accepting writes. Old IDs remain historical, not
replayable. Arbitrary unmanaged whole-store rollback is outside this contract.

### Assets, retention and independent custody

Publish an external result as: bounded private temp file → qualified file flush → immutable digest-bound rename →
qualified directory-entry persistence → SQLite reference/event commit → durable advertisement. A DB sync does not flush
an unrelated asset. Check hash and length; missing/corrupt bytes yield `result_unavailable`, never reconstructed
success. Qualify macOS full-sync and directory behavior separately rather than assuming one syscall works for every
descriptor.

GC first durably removes eligible references, then deletes unreferenced assets after a grace period and directory sync.
Serialize it with publication or recheck under equivalent coordination. Explicit scope references protect inputs needed
by live/unresolved scopes independently of result ACK and history expiry; age alone does not make them orphaned.

Proposed retention is 30 days, at least 24h after ACK, oldest eligible first under a 1 GiB soft budget. Proposed
per-attempt output caps are 16 MiB stdout plus 16 MiB stderr, including assets in accounting. These are not installed
settings. Unknown history can expire without changing custody. Retain only bounded live/unresolved scope facts until
reconciliation; schema references must permit detached historical summaries instead of trapping all events indefinitely.
A full store refuses new intents but continues cleanup and bounded status access. Late append/GC cannot recreate pruned
attempts.

Keep immutable domain/inode bindings, controlled bootstrap identities and every descriptor custodian recoverable in the
existing native registry until closure. SDK `120767d` separates retained metadata from closed-payload compute capacity;
the registry still needs a namespace bound. Publish new launch/recovery custody before acquisition or STOP can take
effect. A retained record is neither permission to signal its saved PID nor an additional scheduling authority.

Corrupt SQLite must not erase the only map of live scope/domain ownership. Preserve old bytes and registry, establish
sole recovery authority, reconcile native resources, then decide whether a fresh store can accept new IDs. Missing or
corrupt registry data is not empty custody. Quarantine/restore, legacy migration, unidentified affected domains and
namespace pressure remain explicit review gates, not permission to delete a live database. Missing old results do not
block new admission once resources are established free.

### Linked SQLite provenance

The [WAL-reset advisory](https://sqlite.org/wal.html#the_wal_reset_bug) identifies fixes in 3.51.3 and later, with
backports 3.44.6 and 3.50.7. FULL does not replace that fix. Require a fixed build or verified vendor backport for
**every** runtime accessing the proposed store, including checkpoint/read/maintenance connections. One logical writer
does not exclude a concurrent checkpoint connection.

Record runtime `sqlite_version()`, `sqlite_source_id()`, compile options, wrapper version, selected VFS, executable and
static/system linkage or actual loaded-library provenance. Cargo.lock or an unrelated `sqlite3` executable is not that
evidence. Compare versions semantically, allow documented backports, recheck packaged images/connection factories and
exclude unqualified tools from the live store. Disposable stress tests complement verified build provenance; they cannot
prove this rare race absent. No observed production corruption is claimed here.

## 6. Protocol and state transitions

Public operations are `discover`, `create`, `start`, `get`, `cancel`, `ack`; the ordinary library exposes
submit/get/cancel and hides sequencing. IDs bind store UUID, acceptance epoch and nonce. Mutation checks the persisted
capability; logs/diagnostics redact it. Private producer roles and scope/owner/generation/event bindings authenticate
observations; caller JSON cannot manufacture a native spawn, wait or closure witness. `ea996dd` tests client/private API
separation, not production producer authentication. Owner renewal is a private monotonic generation-bound control
operation.

The display sequence is:

```text
Created → Queued → Admitted → Preparing → Ready → LaunchAuthorized → Running → Stopping → Closed
```

Keep orthogonal facts rather than one cross-product enum: execution authorization/spawn/unknown, result
pending/success/failure/unknown/unavailable/expired, resources unadmitted/held/stopping/closed/recovered/uncertain and
owner observations. `result=unknown, resources=recovered` permits fresh normal admission;
`result=success, resources=held` does not permit a conflicting writer. Preparation can elaborate Lean and perform IO;
its occurrence does not establish target launch or target non-launch.

Persist complete original intent before helper spawn/admission. Register exact helper identity before admission,
including lock waits; register accepted custody before GO. Validate staged inputs and the complete domain union before
Ready. Planned domains are not evidence of acquisition. The service durably consumes launch permission before sending
GO; a private one-use sender/receiver gate prevents a second payload. Repeated START only reports current state. SQLite
commit and OS spawn are separate: a crash after GO but before durable spawn observation remains unknown, and restart
never resends uncertain GO.

The sender irreversibly chooses PossibleGO or SealedNoGO. Persist PossibleGO before the first attempted launch byte;
partial write or ambiguous persistence is unknown. NoDispatch needs SealedNoGO before any GO attempt, or an
authenticated receiver-sealed NoGO under the exact attempt/stage/generation binding. Missing output, lost reply or
preparation ACK is not that seal. Cleanup authority permits stop/reap, never a fresh GO. At-most-one launch within an
attempt does not promise exactly-once arbitrary effects across different explicit attempts.

Record producer role and original scope binding on each observation. Distinguish controller-reported actual wait,
native-owned successful `Child::wait`, helper alias release, helper reap and protected-group closure. An outer wrapper's
`started` frame is not final exec/body/semantic evidence. Lost wait authority or failed publication stays unknown.

GET is read-only and bounded by bytes, pages and time. Its cursor binds the original attempt and a consistent
observation view/version; truncated output is explicitly incomplete. Return complete intent through bounded pages or
retained objects, with explicit expired/unavailable/corrupt states. GET never creates, launches, signals, ACKs, clears
custody, resets idle time or rewrites an old result.

Cancel is idempotent: acceptance of a stop request is distinct from observed closure. Before ACK, the caller durably
saves verified result bytes or an independently retained reference, digest, original binding and relevant status. A
native-store reference alone is insufficient if ACK makes it eligible for deletion. Retry a lost ACK only against that
saved record; a crash before saving leaves it unacknowledged. ACK affects result retention, never custody or verdict.

No new claim/launch authorization commits after its expiry. Expiry cannot revoke an already-authorized GO in flight or
prove cancellation; the bound receiver still enforces original deadline/cancel checks. Late observations cannot revive
an ID, and result-retention expiry never means stopped, successful or replayable. Publication/GC ordering is atomic
where specified, independently of OS delivery.

## 7. Deadlines, inactivity and defaults

All values below are **proposed configurable defaults**, not installed settings.

| Policy | Proposed default | Contract |
| --- | --- | --- |
| Service authority and supervisor liveness | Renew every 10s; expire at 60s | Independent guardian enforcement; expiry requests cleanup, not lock stealing. |
| Admitted idle custody | 30s without an active stage | Polling/heartbeats do not reset idle time. |
| Admission | 5 minutes | Queue, complete-domain acquisition and retries share the original deadline. |
| Active work | 10-minute activity-updated soft bound; 30-minute immutable hard bound | Explicit finite workload profiles may override defaults. |
| Stop | TERM; 5s grace; KILL where identity permits | A sent signal is not exit; API may return cleanup pending. |
| Service restart intensity | 3 abnormal restarts/60s | Back off and report unhealthy. |
| Cleanup observation | Start at 1s; cap at 10s | Bounded retry on affected domains. |

Bind attempt/stage and hard deadline to the guardian before GO. Heartbeats cannot extend it. If cooperative request
cancellation cannot establish safe completion, hard expiry stops the warm generation, retains custody through closure
and requires a fresh generation before another request. Warm idle scope retention follows cache/demand policy rather
than an invented whole-proof lifetime. Client/MCP disconnect alone does not abandon accepted work; owner loss, explicit
cancel and finite deadlines do.

Live activity updates are a required deterministic policy, not historical p99 prediction or a learned scheduler:

- Sample the known owned attempt scope every 10s outside Lean. A new authenticated checkpoint/phase sequence or at least
  1s aggregate owned CPU increase in the last 60s counts as activity. CPU activity is inference, not useful-progress
  proof. Stdout, GET, heartbeats and repeated checkpoints do not count.
- Baseline CPU at request dispatch. Track birth identities, newly forked descendants and final exited-child usage
  without double-counting child totals. Retain exited contributions; PID reuse, sample loss, incompatible counters or
  generation change cannot reset the cumulative counter. Do not count prior requests or background prewarming.
- Incomplete/unattributable accounting is `unavailable`, not zero. Authenticated checkpoints may still count; otherwise
  report the limitation and fall back to the immutable hard bound.
- Activity updates only the subordinate soft deadline to `min(now + 10 minutes, hard maximum)`, through monotonic
  attempt-bound messages. It cannot revive an expired/stopping attempt. Guardian enforcement does not require SQL.
- Inactivity expiry is an operational timeout, not proof failure or deadlock. Quiet CPU-active work continues; spinning
  work reaches the hard maximum. Slow external IO can need a profile with longer/disabled soft expiry and a finite hard
  bound. Observation failure cannot fabricate inactivity or grant indefinite extension.

Complete fork/exit accounting, silent/IO-heavy workloads, error paths and sleep/resume remain qualification gates.
Static hard-bound fallback is not completed activity-based updating. Use monotonic time within a boot; wall time is
diagnostic. On restart reconcile/stop old scopes rather than reconstructing renewable authority from wall time. After
wake, expired owner authority stops work before new GO; no cleanup guarantee applies while the machine sleeps.

## 8. Self-healing and pragmatic resource recovery

Separate whether an old command ran, whether matching work remains and whether new conflicting work can acquire the
resource. Unknown history alone is not a permanent fence.

1. Establish sole recovery ownership through the SDK's short registry publication/arbitration protocol. Do not hold a
   registry-wide lock, DB transaction or the work queue while waiting for cleanup.
2. Retire an original scope restriction after a verified boot change or complete same-boot absence of its payload,
   launchers and custodians, rechecked against concurrent publication. Keep the old outcome unknown. Fully dead scopes
   need not reopen deleted paths or acquire locks held by unrelated new work; successors use ordinary admission. Numeric
   PID/PGID reuse cannot reserve resources forever.
3. For an eligible stopped guardian, prefer resuming its normal drain while custody remains held. Eligibility follows
   cancel, deadline, retirement or confirmed service loss. Trusted post-exec token capture, scope admission and boot
   binding are prerequisites; supplied-token tests do not establish durable discovery. No raw-PID fallback.
4. Qualify takeover separately when payload work survives without a responsive custodian. Reader recovery may acquire a
   fresh validated complete SH union after candidate EX/partial locks close. Recovery-only admission versus waiting
   writer demand is unresolved; it must not permit ordinary reader starvation/bypass or payload execution under cleanup
   authority. Exclusive-writer takeover needs its own contract.
5. Publish recovery intent/member identities before STOP and register recoverer descriptor custody before acquisition.
   Exact tokens identify individuals, not descendant sets. Concurrent fork/exec, anchor loss, incomplete census, two
   recoverers, crash cuts and a frozen recoverer require explicit bounded ownership/arbitration. Do not hide another
   custodian or permanent controller behind a recovery API.
6. Release takeover protection only after its required payload/launcher/custodian closure observations. Bare EX, signal
   return zero, ACK or missing SQL rows do not establish closure. Preserve the managed-writer owner-record gate. Missing
   old wait/result receipts do not block resources already established free.

Record the evidence behind `recovered_after_owner_loss` without inventing original success. Repeated matching-identity
observations, trusted membership, boot transitions and lock facts have different roles. Stale-token rejection alone is
not absence; a prospective exit watch is not late attachment to an old PID. A bounded reconnect may preserve a final
receipt after service death, but failed publication cannot prolong established-closed custody.

Proposed cleanup uses one 5s software budget with bounded backoff and explicit pending reasons, without promising to
bound hung kernel calls. Restrict unresolved scopes to their actual domains and continue unrelated work. Unkillable
work, missing OS observation, lost membership and simultaneous anchor loss remain explicit scoped limitations. The SDK
API is scope-ID plus eligible trigger → recovery observation; the host schedules retries. No second queue, cron or SQL
recovery inventory. macOS permissions/private APIs and Linux behavior qualify separately.

## 9. Proposed file changes and removals

The [cutover ownership table](native-execution-cutover-plan.md#verified-ownership-and-deletion-targets) is canonical for
module/caller changes and deletion counters. New service/protocol/store module placement remains an interface review
decision; existing test modules are not production implementations. Delete superseded controller and capacity authority
after parity, preserving useful native mechanisms and historical receipt readers.

## 10. Migration and rollback

Follow the [cutover checklist](native-execution-cutover-plan.md#staged-acceptance-and-deletion-checklist) and
[rollback contract](native-execution-cutover-plan.md#receipts-and-rollback). They own launcher closure, capability
epochs, matched images, maintenance and deletion evidence. New checks cannot retroactively fence unchanged binaries;
preserve canonical lock inodes and current custody throughout. No deployment or push is authorized by this document.

## 11. Acceptance and fault matrix

The **complete integrated-service matrix remains NOT RUN**. Private checks cover their named prototype cases only.
Freeze source/dependency/lockfile revisions, exact image hashes, OS and fixtures; use causal barriers and independent
oracles. Failed setup is NOT RUN. No test-feature binary silently qualifies a different deployed image.

| Area | Required cuts and oracle |
| --- | --- |
| Identity and state | Lost CREATE/START replies, immutable-intent conflict, stale epoch/generation/capability, one-use GO, sealed cancel, bounded queues, atomic event/state updates and expiry without resurrection. |
| Launch | Before helper identity/admission, after wrapper spawn, Ready, durable permission, first/partial GO, spawn/exec/body/result and ACK. At most one target marker; witness loss stays unknown. |
| Custody | Service/supervisor/guardian crash or freeze, open stalled channel, simultaneous owner loss, leader exit, fork/exec, detached/regrouped or FD/stdio-closing descendants. Assert actual no-overlap and closure or state unsupported containment. |
| Recovery | Reused PID, boot change, stale row/free lock, busy lock/missing row, replaced inode, partial unions, missing guardian reply and competing/frozen recoverers. Successors progress after established closure without invented old results. |
| Storage and control | Full/busy/read-only/corrupt store, failed write/sync/commit/rename/checkpoint/GC, duplicate observations, invalid producer/capability, FIFO/symlink/oversized input and unavailable socket. Verify the injected fault occurred; cleanup cannot require successful publication. |
| Time and activity | Delayed/late renewals, expiry race, idle hold, quiet long work, CPU spin, IO profile, fork/exit accounting, unavailable observation, sleep/resume and TERM-resistant children. No renewal extends hard lifetime. |
| Regression and workflow | Canonical physical domains, formatter inherited group/no-build behavior, FIFO metadata, warm reader retirement, foreign writer, full-union retry and one payload after generation replacement; readiness/build/metadata/axioms/consumer/lint/semantic/final-input acceptance. |
| Cutover | Verify the canonical plan's stale-launcher refusal, old-scope quiescence, singleton/epoch and unchanged-inode checks, then each deletion counter. |

Report implemented, reviewed, private-tested, deployed and production-observed columns separately. Unit/model results do
not replace exact-image faults. Tooling success does not close a mathematical proof claim or change theorem scope.

## 12. Limits

Fail-stop recovery may terminate useful work. No component promises an outcome after every witness dies, exactly-once
arbitrary IO, forced release by an unkillable process or exclusion of unmanaged writers. Qualify only the supported
platform/process/storage profile and keep skipped or unresolved cases visible. The remaining mechanisms are the ordered
gates in §16, not additional rival architecture checklists.

## 13. Validation and refinement

Give CREATE durable acceptance, START one-use authorization and CANCEL request acceptance distinct linearization points;
CANCEL is not closure and no point atomically joins SQL and spawn. Use
[Herlihy and Wing](https://www.cs.columbia.edu/~wing/publications/HerlihyWing90.pdf) for that sequential contract and
[Lamport](https://lamport.azurewebsites.net/tla/book.html), chapters 3–5, 7–9 and 14, for refinement, time, fairness and
TLC.

The bounded specification separates OS operations from persistence, with crash cuts between them. Start with two
attempts/domains, one warm generation, owner failures, aliases, live work, epochs, intent/results and lost replies.
Check no conflicting work overlap, at-most-one launch, no expired revival, custody through live work and unknown history
coexisting with recovered resources. Conditional progress requires explicit responsive-actor, OS-observation and
availability assumptions; never assume a crashed actor runs or an unkillable child exits. Warm requests cannot inherit
prior CPU activity or renew each other's deadlines.

Map internal steps to §6's orthogonal public facts; include broken early-release/GO-replay variants that must produce
counterexamples. Record tool/model revision, bounds, explored states, assumptions and traces. Bounded checking is not an
unbounded proof. Translate counterexamples into executable fixtures. The
[failure-detector distinction](https://ecommons.cornell.edu/entities/publication/7948ff49-7263-49f8-a29b-d062e7cbb240)
also applies locally: lease expiry is suspicion triggering cleanup, not proof of death.

Qualify OS assumptions with disposable competing commands, independent write intervals and scope-closure observations.
Existing macOS leader observation/pinning and guardian checks remain narrow. Candidates such as
[launchd group cleanup](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5),
[Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html) and
[pidfds](https://man7.org/linux/man-pages/man2/pidfd_open.2.html) need current platform/permission tests. Identity-safe
signals do not contain descendants; eventual cleanup does not establish lock retention until exit.

Use consuming non-Clone launch tokens and explicit descriptor states where helpful, without a giant typestate product.
[`OwnedFd`](https://doc.rust-lang.org/std/os/fd/struct.OwnedFd.html) ownership does not eliminate kernel aliasing;
[`Drop`](https://doc.rust-lang.org/nomicon/leaking.html) is not guaranteed. Tokio cancellation/kill requests are
distinct from [observed child completion](https://docs.rs/tokio/latest/tokio/process/struct.Child.html). Keep
independent guardian enforcement outside blocking work. [Loom](https://github.com/tokio-rs/loom) can check small
synchronization races, not real FDs, process groups or fsync. Independent review must challenge assumptions and
unsupported cases before dependent wiring.

## 14. Model evidence

The [model directory](../specs/native-execution/README.md) owns artifacts, reproduction commands and recorded results.
At port `a309cc6`, its owner records SANY parsing and intended results for all 155 required TLC cases, including exact
negative-control targets. The extended two-message stage case completed with 802,787 distinct states in 28.793s; the
larger case with all controls hit its 30s bound and remains **INCOMPLETE**. The earlier Python exploration stopped at
300,001 states; it is a different incomplete run, not a TLC result. Historical Python tests are not additional TLC or
runtime cases.

The coarse models retain the earlier supervisor/guardian alias arrangement. They do not model the selected SDK
registry/token recovery, a third service alias, boot/exec token capture or takeover. Complete-domain acquisition,
authenticated callbacks, sealed launch gates and observed whole-scope termination remain assumptions. These separate
checks are neither a composition proof nor native recovery qualification.

Custody refinement requires close-only aliases with no live SH/EX mode conversion. This does not assert every failed
nonblocking conversion loses its lock; runtime syscall semantics and storage/process assumptions qualify separately.

## 15. Storage qualification

The [private store contract](../specs/native-execution/stage-store-contract.md) and
[runtime contract](../specs/native-execution/stage-runtime-contract.md) record prototype evidence. Their kill/reopen,
caller-save-before-ACK, rejected SQLite UPDATE cleanup and healthy-guardian/passive-observer cases do not establish
replacement-service adoption or production store/RPC use. Client authority tests are separate from producer bootstrap.
Private schema versions and 64 KiB inline test bounds are not production defaults.

Keep these failure classes and their evidence separate:

| Class | Required qualification and limit |
| --- | --- |
| Process/service death | Disposable kill/reopen at intent, permission, GO, result and ACK; original IDs, no reauthorization, truthful unknown/missing assets and native cleanup. OS/device stay running. |
| Simulated storage faults | Test VFS/adapter verifies write/flush/commit/rename/checkpoint failures and ordering, effective settings on fresh/reopened/pooled connections, bad-profile rejection and runtime provenance enforcement. Passing stress alone does not qualify WAL-reset safety. |
| External assets and GC | Independently test file/rename/directory/DB cuts and publication/GC races, including live-scope input pins. SQLite PRAGMAs do not synchronize these assets. |
| OS crash or reboot | Dirty kernel state loss differs from process kill; boot identity and recovered outcomes require separate evidence. |
| Physical power loss | Device/cache persistence differs again. SIGKILL or VM-process restart does not prove flush durability. Never power-cycle a user's machine for this test; any future destructive test requires an authorized disposable environment. |

VFS injection, external assets, OS-crash and physical power-loss qualification remain unrun in the recorded evidence.
Record SQLite/VFS/filesystem assumptions and weaker supported profiles explicitly rather than claiming universal
durability. Storage failure cannot trap cleanup, fabricate no-start or erase original identity.

## 16. Remaining implementation and acceptance gates

Independent preparation can proceed; dependent production wiring cannot assume an unresolved predecessor passed.
Existing private tests are reusable evidence, not a production implementation.

| Order | Owner and existing boundary | Remaining gate |
| --- | --- | --- |
| 1. Frozen scope and identity | SDK `native_guardian.rs`, `native_guardian/ownership.rs`; host contract §§4,8 | Qualify trusted post-exec token/boot admission, frozen/dead-custodian cleanup, normal CONT drain and supported lost-anchor cases with independent closure/contention oracles. Supplied-token and failed setup evidence is narrower. |
| 2. Registry recovery and admission | SDK `crates/lean-toolchain/src/native_guardian/ownership.rs`, `crates/lean-toolchain/src/writer_lock.rs` and `crates/lean-toolchain/src/native_guardian/record_lifetime_tests.rs` | Specify publication before STOP/acquisition, two-recoverer/frozen-recoverer/crash handling, complete SH recovery versus writer demand and exclusive-writer limitations. No second inventory/controller. Depends on 1 for affected cases. |
| 3. Warm generation and one capacity policy | Host `broker.rs`, `broker/control.rs`, `project.rs` and lifecycle tests | Map attempt/request/generation lifetimes; remove duplicate grants. Test sequential reuse, cancel, retirement, service loss and replacement. One-stage/scope prototypes do not settle warm integration. Recovery-dependent wiring waits for 1–2. |
| 4. Production store, protocol and authority | Host service registration and private store/runtime reference tests | Review production module placement and producer bootstrap; preserve binding/one-use GO/caller-save-before-ACK. Complete corruption/restore, retention, registry pressure and external-asset procedures. No listener or production dispatch is established. Depends on 2–3. |
| 5. Live activity and deadlines | Host configuration; SDK guardian | Implement §7's deterministic accounting, unavailable fallback and independent bounds. Qualify fork/exit, silent/IO work and sleep/resume. Policy is specified; complete platform accounting/enforcement is not. |
| 6. Cutover and deletion | Repository coordinator; canonical cutover plan | Qualify the complete launcher/fence/rollback/deletion inventory after 1–5. Local implementation authorization does not authorize install, deployment or push. |
| 7. Exact-image workflow and proof gates | Coordinator; host `crates/lean-host-mcp/tests/smoke_perf.rs`, `crates/lean-host-mcp/tests/kanproofs_eval.rs` and `scripts/memory_stability.py`; proof acceptance paths in the [cutover inventory](native-execution-cutover-plan.md#launcher-closure-set) | After applicable authorization, run candidate fault/mixed workflows with exact hashes, overlap, memory/swap and cleanup/unknown evidence. Preserve source/theorem/input gates. Earlier finite acceptance is not qualification of this replacement. |

Do not add storage features to postpone custody decisions. Reconcile the smallest qualified recovery contract, then wire
the narrow service/warm path. If an OS counterexample defeats a claim, narrow support or change the mechanism; a timer,
database row or additional owner cannot silently turn the failed case into success.
