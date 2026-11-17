# Native execution lifecycle and self-healing: replacement design

Status: **DESIGN ONLY; proposed, not implemented or qualified.** Written 2026-11-17.
This document selects a concrete architecture for review. It is not authorization to implement or deploy it.
It supplements the historical inventory in `docs/native-lifecycle-evidence.md`; where that document leaves storage
undecided, the recommendation here is SQLite. Existing source scaffolds are not this design's implementation.
No proof inputs, installed binaries, live configuration, lock files or historical receipts change with this document.

## 1. Decision and scope

Replace fragmented execution lifecycle ownership with a native Rust execution service and a small supervised process
boundary. Keep Lean semantic project actors, worker/session reuse, canonical artifact locks and tested process-group
machinery. Do not rewrite Lean, the semantic API, the formatter or proof orchestration wholesale.

The service owns admission policy, immutable attempt identity, deadlines, status and persistence. A native scope
supervisor owns actual command children and cleanup. An independently scheduled custody guardian watches that
supervisor and keeps the artifact exclusion alive until its protected process scope is stopped. This last claim is a
**design obligation**, not a property already established for all existing writers. A supervisor plus a database alone
is insufficient.

Proofs owns source snapshots, theorem scope, proof steps and acceptance gates. It becomes a thin execution client,
not a process supervisor or a second native scheduler. The service must work for other Lean repositories without
IUT names, source layouts, theorem policies or Python registry dependencies.

Success means routine abandoned work recovers automatically, without manual drains or permanent fences caused only
by a missing historical receipt. It does not mean every interrupted operation has a known outcome. Never silently
retry an operation that may have run. A newly authorized attempt can proceed once current resources are available,
even while the old attempt's result remains unknown.

### Design principles and alternatives

Direct local references: `/Users/jcreinhold/Code/papers/logic-and-computation/software-engineering/simple-made-easy/text.md`
(Rich Hickey, especially the complexity table and who/what/when/where discussion), and
`/Users/jcreinhold/Code/papers/logic-and-computation/software-engineering/philosophy-of-software-design/`
(John Ousterhout, chapters 4–8, 10–11, 23–24). These are design criteria, not endorsements of this implementation.

- Separate intent (what), scheduling (when), custody (who currently owns the processes/resources), outcome (what was
  observed), and recovery (what is available now). An actor is an implementation option, not a simplicity proof;
  placing everything in one actor can still couple independent concerns.
- The public module is deep: normal use supplies an operation and gets a durable handle/result. It must not expose
  guardian ACKs, descriptor loans, process-group polling, heartbeats or storage transactions. Pull these obligations
  below the boundary. Native internals may have more code while clients understand fewer concepts.
- Put one policy in one place. Lease expiry, idle release, adaptive default budgets and recovery classification cannot
  be copied into Python, SDK and host. SQLite replacement must not alter admission or process ownership semantics.
- Define avoidable errors away: identical CREATE retry recovers its handle; repeated CANCEL is harmless; GET never
  launches work; absence of an old result never itself holds a current lock. Do not conceal real uncertain outcomes.
- Compare alternatives by independent authorities, facts crossing layers and client obligations, not line count or
  familiarity. Reviewers should be able to explain every remaining process and delete each redundant ownership path.

| Alternative | Benefit | Why not selected / tradeoff |
| --- | --- | --- |
| Add receipts and timers to current Python/helper layers | Smallest immediate edit; reuses current paths | Leaves multiple lifecycle authorities and callers reconstructing phases; repeats information leakage |
| One native service directly owns all children, no independent custody guardian | Fewer processes and simplest normal path | Service death can free locks while descendants continue; acceptable only after a stronger OS containment solution, not by assumption |
| Native service plus minimal independently scheduled custody scope (selected) | Native-only lifecycle policy; clients stop owning helpers; resource custody survives control-plane loss | More internal process mechanics; must prove guardian boundary and delete old controllers, otherwise merely adds a layer |
| Rewrite semantic workers, formatter, queues and execution together | Freedom to redesign everything | Discards validated mechanisms, increases migration surface, and does not by itself solve crash containment |

The selected design remains provisional at its explicit containment gate. If a simpler qualified OS scope can replace
both supervisor and guardian, prefer it. Do not defend this process count merely because it was proposed first.

## 2. Verified baseline and concrete failures

Inspection baseline: host checkout HEAD `64b5b86dbe7c5feff043a9cc3e889d29578dc718`, SDK baseline
`0976c61c65e25d2eaa3c62eb44ec7aafc51e5aa7`. Three pre-existing uncommitted host source scaffolds are preserved.
Historical deployment and acceptance facts below come from recorded artifacts, not a fresh production test today.

| Historical failure or limitation | Repair/evidence | What replacement must retain or add |
| --- | --- | --- |
| Repository queue serialized native work; baseline BGR approximately 598s, 570s waiting | Proofs `135adf6203`, `7b959123c2`; schema-3 migration archived 4,876 native operations | No second queue or durable SQL capacity hold in Proofs |
| `/var` versus `/private/var` smoke identity mismatch | Host `714fc5e`; initial rollout rollback then corrected deployment, Nov 16 | Canonical domains, inode validation, exact capability discovery |
| Metadata FIFO could hang | SDK `b55d935`, host `65223a1` | Bounded, regular-file-only metadata access; no blocking special-file reads |
| Formatter detached children | lean-fmt `d3aaa47`; no-build setup `81899c2` | Managed mode stays inside supervised group; no approximate fallback |
| Explicit dependency domain incorrectly expanded as another workspace | SDK `328a9d6`, host `d1b7f56` | Preserve primary-workspace versus physical-domain distinction |
| Warm readers held SH while foreign writer waited for EX | SDK `c6067f2`, host `2a46a5a` | Writer demand and targeted idle-reader retirement, no global drain |
| Single-domain then multi-domain readers refused during foreign writer demand | Host `e1c8a0f`, SDK `0976c61`, host `5fb7dd8` | One original admission budget; drop the entire partial union before waiting |
| Later strict call `:122` and lint window overlapped a long BranchStar writer | `/tmp/proofs-07m7-current-admission-diagnosis.json`; writer approximately 302.5s, exited compile failure | Contention is not proof failure; phase-specific dispatch facts and bounded admission |
| Writer first locked reply unavailable, no durable helper handle in client | `/tmp/lean-writer-admission-identity-assessment.json` | Attempt identity before helper spawn, recoverable lookup independent of reply delivery |
| Post-ACK reader error cannot distinguish preparation from target launch | `/tmp/proofs-07m7-225-native-assessment.json` | Separate preparation, launch authorization, spawn, outcome and closure |
| Alive idle writer controller can retain EX indefinitely | Current host `shared_writer.rs:1306–1342`: blocking `input.recv()` after `locked`; command timer starts in `execute_with_launcher` | Idle-custody expiry enforced outside controller; this is a code finding, not a newly reproduced incident |

Finite mixed acceptance did genuinely pass: target 44.878s, consumer 45.749s, semantic check 36.965s; axiom payload
lifetimes overlapped, no new swapouts. Evidence: `/tmp/proofs-v1-final-acceptance.json` and
`/tmp/proofs-v1-mixed-whole-union-corrected/independent-audit.json`. These are normal-workload observations, not
qualification of lost-controller, supervisor-crash or all post-ACK lifecycle cases. They cannot justify a universal
"lifecycle complete" claim. Later failures exposed additional ownership boundaries, not necessarily recurrence of
the exact same fixed bug. Host `8aeb25ba2756374835ff721c0417739f99709a33` adds pre-dispatch classification but is
not the proposed lifecycle replacement and does not solve post-ACK evidence or idle custody.

## 3. Current owners and exact paths

All paths in this document are on Jacob's Mac unless marked proposed. Root aliases below are explicit and apply to
all subsequent relative paths:

- `HOST` = `/Users/jcreinhold/Code/lean-host-mcp`
- `SDK` = `/Users/jcreinhold/Code/lean-rs`
- `PROOFS` = `/Users/jcreinhold/Code/proofs`
- `FMT` = `/Users/jcreinhold/Code/lean-fmt`
- `H` = `HOST/crates/lean-host-mcp/src`
- `T` = `SDK/crates/lean-toolchain/src`
- `P` = `PROOFS/scripts/stacks/src/stacks`

| Current code | Current responsibility |
| --- | --- |
| `H/broker.rs`, `H/project.rs` | Semantic broker/project actors, warm workers, deadline/cancellation, pool management |
| `H/tasks.rs`, `H/request_log.rs`, `H/request_journal.rs` | Retained semantic prepare/submit/get/ACK and durable request snapshots |
| `H/shared_writer.rs` | External writer helper, EX admission, command launch, actual child/group cleanup; read-transfer and metadata proxy modes too |
| `T/shared_writer.rs` | SDK writer controller; may retain original borrowed EX descriptor aliases |
| `H/native_reader.rs`, `H/native_reader/control.rs`, `H/native_reader/input.rs` | Reader controller/bootstrap/payload, guardian acceptance, late cleanup |
| `T/native_guardian.rs`, `T/native_guardian/ownership.rs`, `T/process_group.rs` | Native custody, real-child/group identity, stop/wait and descriptor ownership |
| `T/shared_reader.rs`, `T/writer_demand.rs`, `H/artifact_reads.rs` | Read domain union, demand protocol, physical resource access |
| `P/owned_writer.py`, `P/native_reader.py`, `P/native_reader_protocol.py` | Python protocol/controller and actual helper wait ownership |
| `P/orchestration/managed_lake.py`, `P/orchestration/managed_read.py`, `P/orchestration/compilation.py` | Repo build/read stages and proof-specific validation |
| `P/native_history.py`, `P/native_protocol.py`, `P/registry.py`, `P/sqlite_store.py` | Repo provenance, source snapshots and semantic history, not native custody authority |
| `FMT` managed subprocess implementation | Formatter subprocesses inherit managed scope; standalone isolation remains separate |

Existing semantic journal: `PROOFS/.local/lean-host-mcp/requests`, selected by `--request-journal`.
It stores atomic JSON snapshots and has an exclusive broker owner lock. Do not let arbitrary helpers write its schema.
Existing repository database: `PROOFS/.git/stacks-source-reservations/generations/42b7b6a8a0334bca91aac0d23a0bcc75/registry.sqlite3`.
This generation-specific path is evidence of current storage, not a configuration default for the new service.
Its WAL transactions and immutable history tables do not make it the right owner for generic native execution.

## 4. Proposed process architecture

```text
Proofs CLI / other client / semantic host
    | typed local RPC; caller-selected durable attempt ID
    v
native execution service (one lifecycle owner, bounded mailbox, SQLite writer)
    | admit declared complete domains + create attempt scope
    v
Rust scope supervisor --------- private lifeline ---- custody guardian
    | actual child handles                           | independent scheduling
    v                                                | kernel SH/EX custody
command group / Lean worker -------------------------+ protected scope
```

Proposed service command: `lean-host-mcp execution-service`. It is independent of a particular MCP transport session
and must remain available to external build clients when an MCP connection closes. A per-user launchd service on
macOS starts/restarts it; installation is a later separately approved step. Do not silently start multiple copies.
Linux service management is a separate platform qualification, not implied by Mac success.

Service ownership: attempt state, bounded pending admission, fairness policy, capacity requests, lease generation,
SQLite writes. The actor never blocks on child wait, filesystem I/O or lock acquisition. Dedicated bounded I/O and
supervisor workers return typed observations. Preserve existing semantic project actors; current `project.rs` uses typed `ProjectMessage` dispatch through
`ProjectActorState::handle_message`. The repository AGENTS description of a closure-channel-only actor is older than
this inspected code. This design does not authorize gratuitous actor-interface rewrites. New lifecycle protocol uses
typed Rust enums and immutable status values.

Scope lifetime is explicit: an external one-shot command has an attempt scope; a warm semantic worker has a
**generation scope** shared by sequential request attempts, never a new supervisor per request. The execution service
is the sole global capacity/admission policy owner; the existing native guardian-backed slot mechanism represents
physical occupancy and is released only with actual generation closure. Move broker `worker_slots`/`capacity_admission` grant decisions into this sole service policy; remove the
old independent grant authority rather than add another queue; broker retains project/profile selection and warm-cache hints. Project actors
retain their typed-message worker sessions and sequential Lean dispatch, but obtain/release a generation scope via
the service. An active-request timeout stops that generation if Lean cannot cancel safely; no other active request
shares it. Idle generations remain eligible for demand retirement. Exact changes to existing pool/slot code require
an ownership audit before implementation; do not layer a new permit on top of an old independent permit.

Supervisor ownership: command validation, Ready/GO barrier, actual `Child`, group identity, output limits, timeout,
stop escalation and wait. Guardian ownership: retain declared-domain custody and watch the supervisor lifeline;
on supervisor loss, stop its registered protected group and retain exclusion until closure. The guardian, not the
possibly stalled supervisor, enforces expiry: it requires both a current service-issued scope lease and independent
supervisor liveness messages (10s cadence/60s expiry proposed). EOF triggers immediate stopping; an open but stalled
channel expires. Heartbeat handling runs outside the busy Lean actor and outside blocking child/storage work. The
supervisor cannot renew the service authority lease itself, and the service cannot manufacture supervisor liveness.
No extra general-purpose
scheduler in the guardian. Capacity and resource custody must be bound to actual protected scope, not client rows.

The service never executes Lean FFI. Existing per-toolchain workers still own Lean runtime and session caches.
Warm readers keep the current demand-triggered retirement machinery. Independent project operations remain concurrent
within the configured capacity. Proposed pending-admission bound: 64 attempts; reject overload explicitly.
Fairness: FIFO among conflicting pending requests with bounded waiting deadlines, not a claim of global FIFO among
all readers, writers and unmanaged processes. Do not let an endless stream of readers overtake announced writer demand.

**Important unresolved implementation gate:** current reader guardians provide useful primitives, but a generic writer
guardian with safe supervisor-loss cleanup has not been demonstrated. Reuse the custody/group mechanisms only after
an exact ownership audit and fault tests. Payload inheriting a lock descriptor alone is insufficient: it may close that
descriptor and continue accessing artifacts. Guardian must retain its own custody while protected work can still run.
The supported managed process contract forbids detach/regroup and undeclared artifact access. Same-user malicious
processes and arbitrary daemonizing commands are not contained by this design. Stronger arbitrary-process guarantees
would require a different OS containment boundary, not another heartbeat or database table.

### Single-failure custody contract and first qualification gate

Before building the service/store, qualify a **disposable custody fixture** with two fake commands contending for one
canonical scratch artifact lock. It must log actual write intervals independently of the service and demonstrate no
overlap across each injected death. No proof worker, production lock or production database is part of this fixture.
The following is the proposed ownership contract, not a verified property of the current implementation:

- After admission, supervisor and guardian both retain aliases of the actual acquired open file descriptions for the
  complete domain union. Transfer uses explicit descriptor passing/inheritance followed by identity/receipt validation;
  the sender retains custody until receiver ACK. Record every holder. `CLOEXEC` is set by default, cleared only for the
  selected child descriptors at controlled spawn. Payload descriptor retention is not relied on for exclusion.
- Neither owner calls `LOCK_UN`/equivalent explicit unlock on a shared open description: one holder could unlock the
  other holder's protection. Cleanup is close-only; the last alias closes only after protected scope termination.
  Two independent opens are not assumed equivalent to alias transfer. Qualify actual OS lock semantics on each platform.
- **Guardian-only death:** supervisor still has custody and its actual child handles. It stops the protected scope,
  observes closure and releases its aliases; it does not keep serving new stages or release locks merely on guardian EOF.
- **Supervisor-only death:** guardian still has custody and must stop the registered protected scope. The exact mechanism
  for retaining safe group/descendant identity after its actual parent dies remains a BLOCKER. A stale PGID or PID plus
  birth string is not automatically a safe signal capability. Holding locks prevents overlap while that gap is resolved,
  but permanent inability to clean up does not satisfy the self-healing requirement.
- **Service-only death:** surviving scope owners stop under lease/lifeline policy while retaining custody. Database
  availability cannot be required for stop or close. On restart, reconcile scopes rather than silently relaunching them.
- **Simultaneous owner deaths:** neither dual aliases nor SQLite prove descendant closure. Explicit OS containment or
  independently surviving authoritative ownership is required for a stronger guarantee. This remains unsupported until
  qualified, and must not be disguised as a single-failure guarantee.

Design a second disposable fixture for a **direct custody-owner alternative**: a single per-scope Rust process is the
actual payload parent, holds the locks, receives service requests and enforces deadlines, eliminating a separate
supervisor/guardian handshake. Compare owner-only death with surviving descendants. If an available OS containment
primitive supplies a safe owner-death boundary, this is preferable because it removes an owner, descriptor transfer and
mutual heartbeat protocol. Without that primitive it fails the same crucial owner-death case, so fewer processes alone
is not sufficient. Choose the smallest design whose custody fixture passes; do not build the service around an unproven
three-process topology. No fixture implementation or execution is authorized by this design-only edit.

## 5. SQLite decision, paths and schema

Choose SQLite, not a bespoke immutable-file event store. Reasons: one ordered native writer, atomic create/claim/event
transactions, indexed original-ID lookup, bounded retention and fewer publication/expiry races. SQLite does not solve
process custody and is not a lock service. Do not adopt the incomplete Python file-codec overlay.

Proposed macOS state directory: `/Users/jcreinhold/Library/Application Support/lean-host-mcp/execution/`.
Generic default uses the platform per-user application-state directory; explicit `--execution-state-dir` overrides it.
Proposed database: `lifecycle.sqlite3`; same directory has a service singleton `owner.lock` and `assets/<attempt-id>/`.
Use a short per-user runtime socket path selected and reported by discovery, rather than exceeding Unix socket path
limits with the application-support directory. Discovery returns full endpoint, store identity and protocol version.
Directories mode 0700, database/private manifests mode 0600; reject symlink/special-file substitutions. Local socket
checks same-user peer identity. This is local integrity, not protection against a hostile process running as that user.

Proposed tables (schema version 1):

- `store_meta`: schema version, randomly generated store UUID, migration version.
- `service_generations`: generation UUID, boot identity, process identity, start/stop facts.
- `attempts`: service-issued acceptance epoch plus caller-generated random nonce (composite primary key), immutable canonical intent bytes and digest,
  operation kind, exact command/input binding, domain manifest, requested budget, creation time and retention class.
- `attempt_state`: attempt ID, service generation, lease generation, phase, observed status and revision. This is a
  transactional projection, not authority to free resources.
- `scopes`: independently allocated scope UUID (primary key), service generation, role, supervisor/guardian/group
  identity including boot and process birth, declared domains and optional parent scope. A warm generation scope
  outlives individual request attempts; it is not keyed by an attempt. Identity observations are distinct from custody.
- `attempt_scopes`: attempt ID, scope UUID, stage ID and association role, with finite request deadline and terminal
  association status. Multiple sequential attempts can reference one warm generation scope; a request may reference
  preparation and target scopes. The association does not create an additional owner or capacity permit.
- `events`: attempt ID, sequence, unique producer-generation/event ID, event kind, bounded payload and observation
  timestamps. Append-only; retries of the same event ID deduplicate; conflicting same-ID content is rejected.
- `results`: attempt/stage ID, result kind, bounded inline body or immutable asset digest, publication status.

Use foreign keys, WAL, synchronous FULL and one service-owned writer connection. Short transactions; proposed busy
budget 1 second, charged to the original request budget. Event insertion and state projection update are atomic.
No helper writes SQL and no UPSERT recreates an expired attempt.

**Original-job binding precedes CREATE.** Persist the unique binding key `(original job ID, stage key, explicit execution revision)` with immutable
associated values `(input digest, store UUID, epoch/nonce, capability)` durably in the caller's existing provenance
store before the first RPC. The input digest is NOT part of the unique key: the same key with a different digest
must fail, not allocate another attempt. In Proofs, add an `outbound_attempt_bindings` table through `P/native_history.py`/`P/sqlite_store.py`; this is caller
provenance, not a native work queue, custody record or semantic ticket. Unique binding keys reject mismatched input.
After caller crash, reload this binding and GET/reconcile the original attempt; never mint a fresh ID merely because
CREATE or START reply was lost. A new execution revision is an explicit new-work decision. Other clients use the same
library contract with their durable caller journal; the library hides fsync/transaction details. Generic callers that
cannot retain an original operation key cannot claim crash-safe resumption. Terminal/expired native history does not
authorize re-CREATE under a new ID automatically.

 Creation with the same ID and different intent is an
error. Discovery supplies the current acceptance epoch. The client library generates and persists both an epoch/nonce
ID and a random mutation capability before CREATE, sending a capability hash for storage. Thus loss of the CREATE
reply cannot lose the capability. Repeating CREATE with identical intent and capability recovers the same entry.
The service rotates the acceptance epoch on a persisted monotonic sequence, normally daily, and on a fresh store;
missing IDs from closed epochs can never CREATE. Persist the next sequence and closure of the prior epoch atomically
before advertising a new epoch. Restart reads the committed current epoch; clock rollback cannot reopen an older
one. Wall time may trigger rotation but never determines epoch identity or reverses closure. A fresh or recovered
replacement store gets a new random store UUID, so restoring or losing a sequence cannot reuse old global IDs.
Backup restore must allocate a new store identity before accepting creates; old IDs remain read-only historical references.
Existing IDs remain GET-able and identical CREATE retries return them even after epoch closure. Retain all deduplication
rows/tombstones for the current open epoch; prune them only once its creation window closes. This prevents an expired
ID from being resurrected after tombstone pruning without an indefinitely growing tombstone set. Original IDs retain
store UUID as well. RUN is not automatically repeated.

**Result assets and publication ordering.** Write a bounded asset to a private temp file in the destination filesystem;
fsync its bytes, rename to an immutable digest-bound name, fsync the containing directory, then transactionally commit
the SQLite result reference/event. Only after commit advertise a durable result. A crash before commit may leave an
orphan asset; a crash after commit must not leave a knowingly unsynced referenced asset. Hash/length checks detect
missing/corrupt bytes; report `result_unavailable`, never reconstruct success. GC first commits removal of eligible
references, then deletes only unreferenced assets after a grace period and directory fsync. Recheck references under
serialized GC/publication coordination; no concurrent writer can attach an asset being deleted. Staged inputs required by live or unresolved resource scopes are protected by explicit scope references independent
of result ACK/history expiry. GC must check these references and current scope reconciliation before removing an
input, even if its attempt history was pruned or result is terminal. Remove those protections only after resource
closure/recovery establishes the input is no longer in use. Recovery may sweep old temp/orphan files with bounded
work, but age alone never makes a live scope's staged input an orphan. SQLite FULL does not fsync an unrelated asset automatically.

**Independent minimal custody inventory.** `execution/custody/<scope-uuid>.json` is a small, versioned scope manifest
separate from SQLite history: store/service generation, canonical domain paths and inodes, boot/process/group identities,
private owner endpoints and declared alias holders. Publish it atomically with file/directory fsync before any GO;
update identity transfer before releasing the corresponding launch gate. It contains no command outcomes, runnable
jobs or general event log. Live authenticated owners and kernel locks remain authoritative. Remove it only after
resource reconciliation/closure, not result ACK or history retention. Thus a corrupt history DB does not erase the
only map from live work to affected domains. The inventory is bounded by admitted scopes plus unresolved live scopes,
not number of historical attempts, and requires explicit size/version/no-symlink limits.

On SQLite corruption, keep the inventory, query its owners, stop/reconcile affected scopes and preserve the corrupt DB.
Block only affected domain unions where their identities/custody remain uncertain; unrelated verified domains may
continue once a new valid store is available. Missing/corrupt inventory cannot be treated as an empty inventory: use
live owner enumeration and stable lock/identity observations; if these cannot establish the impacted domains, disclose
that domain isolation is itself unresolved. The inventory is deliberate minimal recovery redundancy, not a second
execution authority. Its corruption/transfer races must pass the custody fixture before automatic store replacement.

Supervisor reports over its authenticated private channel. If service dies, supervisor stops work instead of maintaining
an autonomous second journal writer. A bounded final-receipt reconnect may improve evidence after service restart, but
failure to publish cannot prolong already-closed resource custody. If all witnesses die before publication, outcome
remains unknown. A new service reconciles current resources separately.

Retention proposal: terminal attempts 30 days, minimum 24 hours after ACK, oldest eligible first under 1 GiB soft budget;
large outputs capped per attempt (default 16 MiB stdout plus 16 MiB stderr), immutable assets included in accounting.
Separate bounded historical evidence from a minimal live-resource registry: unknown/reconciling history ages out
under the same retention policy without changing resource ownership. Keep only currently live or specifically unresolved
scope identity/domain facts in `scopes` until reconciliation, not arbitrarily large attempt outputs. Retention may remove
history while retaining that small scope record; schema references must therefore allow a detached historical summary,
not a foreign key that traps all events forever. Bound live scope admission by capacity and pending limits. At a genuinely
full store refuse NEW intents, continue cleanup and bounded status access; missing old evidence alone cannot fill it
forever. Pruning has a bounded tombstone period and returns `expired`, never `not_started`; no unknown-ID
lookup implicitly allocates work. Closed acceptance epochs prevent re-creation after tombstone expiry. SQLite corruption triggers automatic diagnosis/quarantine of the store only after
service ownership is established, preserving the old bytes; starting a fresh generation requires resource reconciliation,
not inference that the corrupt store held nothing. Exact automated corruption-recovery procedure is an implementation
review gate, not permission to delete this database now.

## 6. Protocol and state transitions

Public operations: `discover`, `create`, `start`, `get`, `cancel`, `ack`. The ordinary client library exposes submit/get/cancel and hides create/start sequencing. IDs always carry store UUID, acceptance epoch and attempt nonce. Mutation requires the capability the client
library generated and persisted before CREATE, never a guessable PID. Logs omit it. Owner renewal is PRIVATE
service/supervisor-to-guardian control, not public client protocol. Service-generated generation and monotonic renewal sequence
reject stale/duplicate renewal.

`Created -> Queued -> Admitted -> Preparing -> Ready -> LaunchAuthorized -> Running -> Stopping -> Closed`.
Any prelaunch state can become sealed `NotStarted`; after authorization but before a spawn witness, execution may be
`Unknown`. Do not implement this display sequence as one enum that conflates independent facts. Typed status has orthogonal
components: `execution` (not-authorized/authorized/spawn-observed/unknown), `result` (pending/success/failure/unknown/
unavailable/expired), `resources` (unadmitted/held/stopping/closed/recovered/uncertain), and owner observations.
For example `result=unknown, resources=recovered` permits new normal admission; `result=success, resources=held` does
not permit a conflicting writer. `Closed` describes resource closure, not successful compilation. Independent fields
track target dispatch, semantic result, command exit, supervisor exit and resource availability. Preparation itself may elaborate Lean.

Commit durable intent before spawn/admission. Register supervisor identity and guardian scope before giving GO.
Supervisor stages inputs and validates complete domains before Ready. Service records LaunchAuthorized before sending
GO; supervisor owns the one-use launch gate. SQLite commit and OS spawn are not one atomic transaction; no schema removes that crash window. Repeated START returns current state, never a second payload. Actual spawn
and wait observations follow. A crash between GO and durable spawn observation remains unknown; at-most-one launch
within a live attempt is the goal, not exactly-once arbitrary I/O across crashes. Service restart never resends GO for
an uncertain attempt. New work requires a new explicit attempt.

Before ACK, the caller durably commits the terminal result or a retained immutable result reference, its digest,
original attempt binding and relevant status into caller provenance. A native-store reference alone is insufficient
if ACK can make its target eligible for deletion: first copy the result to caller-owned durable storage or obtain an
explicit independently retained reference. Lost ACK replies are safe to retry against this saved record; a crash
before that commit must leave the result unacknowledged. ACK releases result-retention obligations only. It cannot
erase current custody or convert unknown to success.
Status GET does not renew a lease or reset idle time. Cancel is idempotent and acknowledged as requested before actual
closure; return terminal closure only after observation or clearly labeled recovery inference.

## 7. Deadlines, inactivity and defaults

All numbers below are **proposed defaults**, not installed settings. They are ordinary configurable policy choices, not a separate broad calibration project.

| Policy | Proposed default | Meaning |
| --- | --- | --- |
| Service/supervisor-to-guardian renewal | every 10s; expiry 60s | Missing live owner causes fail-stop cleanup, not lock stealing |
| Admitted idle custody | 30s | No stage starts: close custody. Heartbeats/status do not reset this timer |
| Admission budget | 5 minutes by default, explicit override allowed | Queue and multi-domain acquisition share the original admission budget; no fresh budget on each retry |
| Active execution | live activity-updated soft deadline; 30-minute hard maximum | Native policy supplies defaults; explicit finite workload overrides |
| Stop escalation | TERM, 5s grace, KILL where owned identity permits | Signal sent is not evidence of exit; bounded API response may report cleanup pending |
| Service restart intensity | 3 abnormal restarts in 60s | Back off and report unhealthy instead of uncontrolled crash loop |
| Cleanup observation | 1s initial interval, cap 10s | Bounded background retry; affected domains isolated |

Lease renewal proves a responsive owner, not command progress. The service renews only accepted live scopes. One-shot attempts stay within their original maximum lifetime;
warm generation scopes are renewed while legitimately retained by cache policy and no retirement/stop has begun,
not given an invented fixed proof lifetime. Their active requests still have finite deadlines. The guardian checks
both service authority and supervisor liveness independently. Before GO, the service also binds the current request's
hard deadline and attempt/stage identity to the guardian. Guardian enforcement is independent of heartbeats: a responsive
service or supervisor cannot keep a hung request alive beyond that deadline. On an active warm-generation request
expiry, stop the protected generation if cooperative cancellation has not established safe request completion, retain
custody through closure, and require a fresh generation before dispatching another request. Ordinary renewals cannot
extend this deadline; a new hard deadline would require a separately specified authorized operation, not a timer reset. The activity policy below may update only a subordinate soft deadline within this immutable hard limit. Losing a transient client/MCP connection does not by itself abandon an accepted job;
explicit cancellation, finite job deadline and native owner loss do. A client-controlled interactive session would need
a separate explicit client lease, not inference from conversation activity.

**Activity-based automatic updates are a requirement, not silently deferred.** Use a small deterministic policy, not a
learned scheduler. Separate (a) owner liveness, (b) recent work activity, and (c) immutable maximum runtime. Guardian
always enforces (c), regardless of either other signal. Values are proposed configurable defaults; testing checks their semantics, not a mandatory statistical justification of every constant.

- Start with a 30-minute hard maximum and a 10-minute soft inactivity deadline. Explicit workload bounds override
  these defaults. Do not add historical p99 estimation, training samples or automatic future-job budget prediction:
  those are separate complexity, not required for the live activity updates requested here.
- During a running operation, sample the known owned process scope every 10 seconds outside the Lean actor. A new
  authenticated phase/checkpoint sequence, or at least 1 second of aggregate owned CPU-time increase during the last
  60 seconds, counts as activity. CPU is a pragmatic activity inference, not proof of useful progress. Neither stdout
  bytes, GET requests, heartbeat packets nor a repeated checkpoint count. CPU accounting is attempt-scoped: establish
  a baseline at request dispatch, never count previous requests or background prewarming in a shared warm generation.
  Track authenticated process birth identities and monotonically accumulated CPU deltas for the protected scope,
  including newly forked descendants and final usage on exit, without double-counting child totals in parent counters.
  Retain contributions of exited children across subsequent samples. A cumulative scope counter must never decrease
  or reset merely because the busy child exited or its PID was reused. The platform implementation must specify how
  it observes fork/exit and obtains final accounting; if it cannot attribute the complete sample to this attempt,
  CPU activity measurement is `unavailable`, not zero. Missing identity, sampling gaps that lose child accounting,
  incompatible counters or generation change invalidate the affected sample. Authenticated attempt checkpoints may
  still supply activity; otherwise fall back to the hard deadline and report unavailable measurement.
- Activity automatically updates the soft deadline to min(now + 10 minutes, hard maximum). Send a monotonic, attempt-
  bound update to the guardian; no update revives an already expired/stopping attempt. No database write is required
  to enforce the current local deadline, though status records the policy/last observation when available.
- At the soft deadline, absent activity triggers stop as a declared policy timeout, not a proof of deadlock or a proof
  failure. Quiet CPU-active elaboration continues; spinning CPU may continue only to the hard maximum. Legitimate
  waiting on slow external I/O can be falsely classified inactive: provide an explicit workload profile that disables
  this soft rule or lengthens it while retaining a finite hard bound. Observation failure is reported distinctly and
  falls back to the hard deadline, not fabricated inactivity or indefinite extension.

Compare alternatives: historical durations alone adjust future jobs but do not satisfy live activity-based updates;
stdout-silence expiry misclassifies quiet Lean work; unbounded heartbeat renewal can retain stuck jobs forever. The
small soft/hard scheme provides live updating while leaving the hard safety limit unchanged. It is not a general progress
estimator and its CPU/checkpoint hooks are an implementation qualification requirement. If these cannot be provided,
report this requirement unimplemented rather than calling the static fallback complete.

Idle custody (30s without an active stage) remains different from active-work inactivity (10m proposed). Neither uses
conversation activity or client polling. A quiet proof is not automatically dead; the configurable inactivity rule is
an explicit operational tradeoff and must be tested on representative silent and I/O-heavy workloads.
Use monotonic timers within a boot. Wall-clock timestamps are diagnostic only. Across restart, conservatively stop
old active scopes rather than reconstruct a renewable deadline from wall time. Sleep/resume requires qualification:
on wake, expired owner lease stops work before any new GO; no promise of cleanup while the machine is asleep.
Do not add a blanket warm-worker eviction that defeats validated caching; demand-triggered retirement remains primary.

## 8. Self-healing and pragmatic resource recovery

Separate three questions: **did the old command run**, **does matching work still exist**, and **can a new conflicting
operation safely acquire the current resource**. Unknown historical outcome is not an automatic permanent fence.

Recovery algorithm, per affected domain union:

1. Start a new service generation only after obtaining the stable singleton lock; verify canonical artifact paths and
   inodes. Never unlink an occupied lock file or create a replacement inode to bypass it.
2. For old nonterminal scopes, query live guardian/supervisor capability channels and practical process identities
   (boot identity, PID plus birth identity, session/group facts). A reused PID is not the old owner. Absence of a receipt
   alone is not evidence of a live owner. An OS boot change proves old local processes cannot remain.
3. If a matching live supervisor/guardian remains, request fail-stop cleanup through it. If unreachable, use only the
   platform identity-safe stop mechanism; do not signal a recycled PID or unpinned stale group number. Poll with backoff.
4. Test normal nonblocking lock acquisition for the COMPLETE union, releasing partial acquisitions on conflict. An actual
   busy kernel lock is evidence of current custody even if metadata is stale. Do not bypass it because a lease expired.
5. Once relevant identities are absent and the stable union can be acquired, mark old *resource state* recovered with the
   observations and confidence basis. Admit new work under those held locks. Keep old command outcome unknown if no
   result witness survived. Never require an unavailable old wait receipt merely to clear a stale logical fence.
6. If a matching protected process is live despite free artifact locks, stop/reconcile it before conflicting work. EX
   reacquisition ALONE is insufficient when a payload can close inherited descriptors and continue artifact access.
   Likewise absence of the supervisor alone does not prove its descendants are gone.

Practical inference is explicitly supported: a boot change, or repeated matching-identity absence plus successful
canonical union acquisition and no registered protected scope, can establish resource recovery without reconstructing
historic exit status. Record `recovered_after_owner_loss`, not a fabricated original success/stopped receipt. A fixed
number of polls is not proof of absence; the platform process/guardian contract defines what is observed.

For the managed no-detach contract, guardian retains custody independently of the payload and checks registered scope
closure. Supervisor death therefore initiates cleanup, not immediate lock release. If guardian ALSO dies, the replacement
must reconcile the surviving group and any registered descendant identities before overlap. Current Mac group pinning
and Linux post-leader-reap behavior differ: **simultaneous guardian/supervisor death without sufficient surviving
identity is an unresolved containment case**. Do not claim universal automatic crash recovery until the exact platform
mechanism and tests establish it. Options must be reviewed: stronger OS containment, a surviving authoritative custody
owner, or explicitly narrower supported process contract. A timer cannot fill this gap.

Operationally, exhaust automated checks and safe cleanup first; report a specific unresolved live/ambiguous scope,
not a permanent blanket HOLD because a JSON field is missing. Continue unrelated domains. A truly unkillable process,
blocked kernel observation or unverifiable escaped process may still need operator action. Incomplete evidence is not
an excuse for indefinite silent retry: status includes cause, last observation, next retry and affected domains.

## 9. Proposed file changes and removals

New module names below are proposals, not existing implementations.

| Repository/file | Proposed change |
| --- | --- |
| `H/execution_service.rs` and `H/execution_service/{actor,protocol,store,recovery}.rs` | Native lifecycle actor, local RPC, SQLite single writer, restart reconciliation |
| `H/cli/execution_service.rs`; `H/main.rs`; `H/runtime_compatibility.rs` | Explicit service/discovery CLI and matched capability negotiation; replace unfinished discovery scaffolds only after review |
| `T/execution_scope.rs`; `T/native_guardian.rs`; `T/native_guardian/ownership.rs`; `T/process_group.rs` | One audited supervisor/guardian scope API; real-child custody, stop/wait and safe platform recovery |
| `T/shared_writer.rs`; `T/shared_reader.rs` | Thin service clients for lifecycle operations; preserve whole-union/demand semantics, eliminate caller-owned helper lifetime after migration |
| `H/shared_writer.rs`; `H/native_reader.rs`; `H/native_reader/control.rs`; `H/native_reader/input.rs` | Replace independently owned controller loops with common scope implementation; preserve actual launch and input-validation invariants |
| `H/broker.rs`; `H/project.rs`; `H/artifact_reads.rs` | Integrate common scope identities/admission; retain semantic actor/session/cache design and demand-driven retirement |
| `H/tasks.rs`; `H/request_log.rs`; `H/request_journal.rs` | Link semantic tickets to native attempts; keep semantic result/ACK meaning; no merge of incompatible IDs |
| `HOST/crates/lean-host-mcp/Cargo.toml`; `HOST/Cargo.lock` | Select reviewed SQLite dependency and protocol dependencies during implementation; no dependency added now |
| `P/native_execution.py` (new); `P/native_compatibility.py` | Thin discover/create/start/get/cancel client; persist original IDs on all errors |
| `P/owned_writer.py`; `P/native_reader.py`; `P/native_reader_protocol.py`; `P/orchestration/managed_lake.py`; `P/orchestration/managed_read.py` | Remove actual native-helper Popen supervision, late daemon waiter, duplicate timeout/classification logic after parity |
| `P/native_history.py`; `P/native_protocol.py`; `P/registry.py`; `P/sqlite_store.py` | Add durable original-job/stage outbound attempt binding as provenance; preserve source snapshots; no native capacity lease or new execution queue |
| `P/orchestration/compilation.py`; `PROOFS/docs/development/lean-check-redesign.md` | Preserve proof acceptance/final input gates; track real rollout evidence separately |
| `FMT` managed subprocess mode | Retain tested inherited-group contract; no unrelated formatter rewrite |

Delete superseded helper protocols and compatibility fallback only when all supported callers migrate. Do not maintain
old and new authorities indefinitely. Explicitly remove controller-side borrowed EX aliases from the new normal path;
otherwise helper closure cannot release parent-held custody. Audit every FD transfer and inherited alias before claiming
this. Existing standalone CLI compatibility may remain a bounded adapter to the service, not a second execution owner.

### Measurable deletion contract

Before implementation, capture a caller/owner inventory and code references. Cutover acceptance requires all of:
(1) zero native-helper `Popen`/late-wait ownership in migrated Proofs adapters; (2) zero independent broker/global
capacity-grant authorities outside the execution service; (3) zero normal-path client-owned borrowed EX aliases;
(4) one native recovery/expiry policy implementation, no Python copies; (5) one compatibility path, with no automatic
legacy fallback on native error; (6) every preserved wrapper exposes a different useful abstraction or is removed.
List deleted functions/files and retained exceptions in the rollout report, with static searches plus runtime tests.
Lines of code may increase inside the deep native module; the measured result must be fewer caller obligations,
independent authorities and cross-layer lifecycle facts. A release that adds the service but keeps all old owners is
not this redesign and cannot pass by demonstrating only successful commands.

## 10. Migration and rollback

1. Independently review this design, then qualify disposable dual-owner and direct-owner custody fixtures FIRST.
   Resolve guardian-only/supervisor-only failure and the platform identity contract before store or production integration.
2. Implement store/protocol and scope machinery behind an off-by-default versioned capability. Model transitions and
   test private fixtures; do not read production history as work to replay.
3. Route one isolated fixture through the new path, then the complete private acceptance suite. Match host, SDK and
   worker image capabilities. Unsupported combinations refuse before dispatch, with no legacy execution fallback.
4. Enforce cutover fencing before migration: new launch gates require the current installation/protocol epoch from the
   sole service incarnation; stale capabilities cannot launch. Pre-change binaries do not understand this epoch, so this
   check alone cannot fence them. Inventory configured executable paths, stop accepting old client submissions, finish
   or explicitly stop all known old helpers, and update every supported launcher before enabling new submissions.
   Keep the SAME canonical lock inodes and common admission semantics throughout. An old binary manually launched
   later remains an unsupported cooperating external writer (locks still protect where honored), not a falsely fenced
   client. If any supported launcher can still execute an unfenced old protocol, cutover fails. Do not unlink/rotate
   artifact locks or rely solely on PATH updates. Single service owner lock plus inventory of old executors is required.
   Migrate callers in a documented maintenance window: finish or explicitly cancel existing native work, preserve
   original receipts, verify closure through normal observation. No global unrecorded drain or blind replay.
5. Deploy matched images only after separate authorization, verify installed hashes and run one newly scoped real
   workflow. Observe concurrent target/consumer/semantic operations and writer-demand recovery.
6. Remove obsolete ownership paths after this gate. Historical registry/schema-3 archives and semantic journals remain
   readable. Old attempts are not imported as runnable jobs; optional read-only evidence references preserve their IDs.

Rollback switches NEW submissions only after new-version scopes close or reconcile; never run two conflicting custody
systems or downgrade a live SQLite schema in place. Preserve new store and binaries for evidence. A failed migration
must report which attempts started and their current state, not assume nothing happened.

## 11. Acceptance and fault matrix

All new-design cases below are **NOT RUN**. Use causal synchronization, not timing-only sleeps. Freeze exact binary hashes,
source revisions, SDK lockfile, OS and fixture. A test with failed setup is NOT RUN, not passed. Keep the proof source claim
separate from successful tooling acceptance.

- State/model tests: durable original job/stage binding across lost CREATE/START replies, immutable intent, mismatched duplicate create, one-use GO, no renewal after expiry, no stale generation
  mutations, cancellation races, bounded mailbox, transactional event/state updates, retention without resurrection.
- Real-process launch cuts: before create reply; after helper spawn; before/after admission; after Ready; after durable
  authorization; after GO but before spawn receipt; payload exits before publication; after ACK. Assert target marker
  count at most one and preserve unknown where the cut destroys evidence.
- Self-healing: controller hangs with channel open; service SIGSTOP/crash; supervisor SIGSTOP/crash; guardian crash;
  simultaneous service/supervisor/guardian loss; leader exits with descendants; descendant closes stdio; descendant
  closes inherited lock FDs and keeps writing; attempted detach/regroup. Assert no conflicting writer overlap or mark
  the case an unsupported containment boundary. Never count a timeout response alone as successful cleanup.
- Identity/recovery: reused PID, boot change, stale row with free stable lock, busy lock with missing row, replaced inode,
  missing guardian reply, multiple-domain partial acquisition. Show automatic successor progress once matching live
  custody is gone without manufacturing an old result.
- Storage/control: result fsync/rename/commit/GC crash cuts, custody inventory independent of corrupt SQLite, full disk, SQLite busy/corrupt, service loss during commit, duplicate observations, oversized frames,
  invalid capability, FIFO/symlink inputs, unavailable socket, pruning versus late report. Cleanup must not depend on a
  successful database write.
- Time: delayed renewals, renewal/expiry race, 60s owner expiry, 30s idle expiry, sleep/resume, wall-clock jumps, quiet long
  proof within budget, live activity updates bounded by hard deadline, CPU spin, quiet I/O profile, observation failure, TERM-resistant child and KILL escalation. No heartbeat renews maximum lifetime.
- Migration/deletion: stale supported launcher rejected, old helper quiescence, common unchanged lock inode, singleton service generation, and every deletion-contract counter verified.
- Existing regressions: physical-domain canonicalization, formatter inherited process group, FIFO metadata, warm reader
  retirement, foreign writer, whole-union retry, one actual payload after warm-generation replacement.
- Full workflow: readiness, guarded compilation, metadata, axiom checks, importing consumer, lint, semantic checks,
  unchanged final complete-input validation; concurrent mixed workload; resource pressure measurement. No source claim
  closes merely because tooling passes.

Publish a matrix with implemented/reviewed/private-tested/deployed/production-observed columns. Claim completion only
for the supported platform and process contract whose rows passed. Unsolved cases and skipped tests remain prominent.

## 12. Limits and review decisions

This is deliberately fail-stop, not transparent continuation. A service failure may terminate useful work; the benefit
is one recoverable owner and no uncertain automatic replay. Durability cannot promise a result when all witnesses die
before publishing. A timeout cannot force an unkillable process to release a kernel resource. SQLite cannot prevent an
unmanaged Lake process from writing without the locks. Exact-once external effects are not promised.

Before production implementation, the disposable fixture and independent review must settle: (1) guardian custody and platform-safe group cleanup after
supervisor death, including simultaneous death; (2) exact process identity/absence evidence used for automatic recovery;
(3) native semantic actor integration without redundant capacity owners; (4) SQLite corruption recovery and bounded
retention details; (5) concrete service installation and compatibility scope. The proposal chooses Rust native ownership,
SQLite, finite deadlines and recovery independent of historical result, but does not label these unresolved mechanisms
as solved. No code, deployment or proof-worker changes are authorized by this document.

## 13. Research-backed validation plan and implementation handoff

Added 2026-11-17 after primary-source research and independent review. Jacob has now authorized design, review,
refinement and implementation in new sessions. This document remains a proposed design, not a claim of implementation
or qualification. That authorization does not waive the containment gates or authorize deployment/pushing. This edit
adds documentation only. The source-scaffold description in §2 is the historical inspection baseline, not a claim that
those uncommitted files still exist; cleanup owns the current tree inventory.

### Priority 1: specify the public contract and model custody before service implementation

Use [Lamport, *Specifying Systems*](https://lamport.azurewebsites.net/tla/book.html), chapters 3–5 and 7
(specification/refinement), 8 (fairness), 9 (time) and 14 (TLC). The
[PlusCal introduction](https://lamport.azurewebsites.net/tla/tutorial/intro.html) is an optional notation aid, not a
second specification to maintain. [Session 9: Liveness](https://lamport.azurewebsites.net/tla/tutorial/session9.html)
explains why mutual exclusion alone permits a useless system that never makes progress. Require both safety and
conditional progress. [Herlihy and Wing, *Linearizability*](https://www.cs.columbia.edu/~wing/publications/HerlihyWing90.pdf)
provides the sequential-operation contract: identify CREATE's durable acceptance point, START's single-use authorization
point and CANCEL's request-acceptance point. CANCEL acknowledgment is not scope closure. None of these points makes
SQLite commit and OS spawn atomic.

**Required artifact:** a small TLA+ lifecycle/custody specification and checked TLC configurations, proposed location
`HOST/specs/native-execution/{Lifecycle.tla,Lifecycle.cfg,README.md}`. No such new model is claimed to exist or have run.
Start bounded: two attempts, two artifact domains, one warm generation, service/supervisor/guardian identities and
failures, holder/alias sets, protected live work, epochs, durable intent, result knowledge, deadlines and lost replies.
Separate all OS operations from persistence transitions; include a crash between every meaningful pair. Do not model
“spawn-and-record” or “kill-and-close” as atomic actions merely to obtain a passing check.

Check these properties explicitly:

1. No conflicting protected work overlaps, even after a controller dies and the payload closes inherited descriptors.
2. An attempt launches at most once; expired attempts and closed epochs cannot be revived by stale messages.
3. Resource custody is not released while its protected work can still run, under stated platform assumptions.
4. Unknown historical result can coexist with recovered resources and does not itself consume a capacity slot.
5. Eligible healthy work eventually runs or receives explicit deadline rejection; abandoned killable scopes eventually
   release custody. State fairness, eventual OS observation and eventual availability assumptions. Do not assign fairness
   to crashed actors, or assume unkillable processes eventually exit.
6. Warm generation reuse cannot let one request renew another request's deadline or inherit prior CPU activity.

Define a refinement mapping from the internal protocol to §6's orthogonal public execution/result/resource states.
Internal recovery steps may leave public state unchanged, but cannot invent success or erase uncertainty. Include
negative variants that deliberately release custody early or resend GO; TLC must find counterexamples, demonstrating
that the asserted property actually detects the intended failure. Record model revision, tool version, bounds, explored
states, assumptions, counterexample traces and checked properties. A bounded model check is not an unbounded proof.

The official [TLA+ transaction-commit example, `TwoPhase.tla`](https://github.com/tlaplus/Examples/blob/master/specifications/transaction_commit/TwoPhase.tla)
is a useful example of concrete protocol versus abstract contract and refinement, not a recommendation to add two-phase
commit to this service. [Newcombe et al., *How Amazon Web Services Uses Formal Methods*](https://www.amazon.science/publications/how-amazon-web-services-uses-formal-methods)
provides practical motivation for design checking; convert discovered traces into executable fault fixtures rather than
treating modeling as a substitute for implementation tests.

[Chandra and Toueg, *Unreliable Failure Detectors for Reliable Distributed Systems*](https://ecommons.cornell.edu/entities/publication/7948ff49-7263-49f8-a29b-d062e7cbb240)
provides the useful distinction between suspicion and actual failure. Lease expiry triggers cleanup; it is not proof
of process death. This is conceptual guidance, not a reason to introduce consensus machinery on one machine or keep
an indefinite evidence-only fence.

### Priority 2: resolve the OS custody assumptions with disposable competing fixtures

Run the §4 disposable fixture and compare mechanisms BEFORE choosing the full service topology. The model's kernel
holder/identity rules are assumptions until these platform tests support them. Test guardian-only, supervisor-only and
simultaneous death, descriptor transfer before/after ACK, payload closing descriptors and stdio, concurrent fork, leader
exit, and conflicting successor acquisition. Instrument independent write intervals and actual scope closure. A signal
returning successfully is not the oracle. Record safety and recovery outcomes separately.

- **macOS existing code:** inspect and retain the exact guarantees in `T/process_group.rs` (leader observation/pinning
  using `waitid` with `WNOWAIT`) and `T/native_guardian.rs` (`validate_pin`/`kill_payload` and same-session/separate-group
  assumptions). [Apple's kqueue manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kqueue.2.html)
  describes observation, not process-tree containment or safe signaling authority. It is archived documentation; qualify
  against the installed supported macOS release.
- **macOS alternative:** [Apple's published launchd manual](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5)
  describes cleanup of the job's remaining PGID unless `AbandonProcessGroup` is enabled. Investigate a launchd-owned
  direct executor, but verify current behavior and whether private sessions move work outside the relevant PGID.
  Crucially, eventual group termination after owner death does not prove the absence of a lock-release-before-exit
  overlap window. The fixture must test that ordering. Do not claim launchd solves artifact custody without it.
- **Linux alternative:** [kernel cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html) offers a candidate
  per-scope containment boundary: controlled membership before execution, `cgroup.kill`, then `cgroup.events:populated`
  observation. Prevent new entries after emptiness and prevent managed payload escape. Verify delegation permissions
  and supported kernel versions. [pidfd_open](https://man7.org/linux/man-pages/man2/pidfd_open.2.html) and
  [pidfd_send_signal](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html) improve identity-safe process
  operations, not descendant containment. Current Linux SDK restraint after leader reaping must not be replaced by
  a retry loop that signals a possibly reused PGID.

[systemd's killing contract](https://www.freedesktop.org/software/systemd/man/262/systemd.kill.html) and
[its implementation](https://github.com/systemd/systemd/blob/main/src/core/unit.c) demonstrate the distinction between
main-process and group cleanup. Borrow `request_stop` versus `observe_scope_empty`, not the entire service manager.
Likewise [s6-supervise](https://www.skarnet.org/software/s6/s6-supervise.html) and
[s6 status](https://www.skarnet.org/software/s6/s6-svstat.html) are comparators for a small actual-child owner and explicit
cleanup-in-progress. They do not prove this application's artifact-lock safety under supervisor failure, and their
restart behavior must not be copied for commands with unknown outcome.

### Priority 3: narrow Rust guarantees, durable protocol and independent review

Only after the model/fixture resolves the supported custody contract should the new sessions implement the narrow
scope API, service/store and caller migration. Use these sources to make specific implementation obligations explicit:

- [Rust typestate](https://doc.rust-lang.org/embedded-book/static-guarantees/typestate-programming.html): consuming,
  non-`Clone` launch tokens and custody-transfer states can prevent local reuse. Avoid a giant cross-product typestate
  for orthogonal outcome/resource states. Types do not prove process survival or cross-process durability.
- [Rust `OwnedFd`](https://doc.rust-lang.org/std/os/fd/struct.OwnedFd.html): cloned descriptors can share an underlying
  open file description. Rust ownership of one FD is not exclusive ownership of the kernel lock. Keep alias accounting
  and close-only rules explicit. [Rustonomicon, Leaking](https://doc.rust-lang.org/nomicon/leaking.html): destructors are
  not guaranteed to run; `Drop` is best-effort cleanup, not the custody proof.
- [Tokio shutdown](https://tokio.rs/tokio/topics/shutdown), [`Child`](https://docs.rs/tokio/latest/tokio/process/struct.Child.html)
  and [`spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html): cancellation notification is
  separate from observed task/child completion. Dropped children can continue; a kill request is not a wait result;
  direct-child exit is not descendant closure. Keep guardian deadline/liveness enforcement independent of busy Lean
  actors, database work and executor congestion. Bounded dedicated OS/I/O work is not an excuse to hide infinite
  supervisor loops inside uncancellable blocking tasks.
- [Loom](https://github.com/tokio-rs/loom): check small Rust synchronization boundaries such as GO-versus-cancel,
  transfer-ACK/last-alias arbitration and duplicate publication. It does not test real descriptors, SQLite fsync,
  process groups or power-loss durability. Use model-specific primitives and retain actual process tests.
- [Temporal's activity crash example](https://docs.temporal.io/activity-definition): completing work then losing the
  completion report is a real ambiguity even in durable systems. Temporal's retry/idempotency model is not our default.
  Preserve original identity for control retries; keep unknown outcome and recovered resources separate; do not add
  a workflow engine or automatic payload replay.

### Review checkpoints, evidence and stopping rules

The simplicity criteria in §1 remain the architectural filter: independent concerns, a deep minimal client API and
measurable removal of old authorities. Adding a model, SQLite or actors is not itself simplification. Review the model,
then custody fixtures, then scope API/storage, then migration as separate evidence-bearing checkpoints. Independent
review should challenge the assumptions and attempt counterexamples rather than merely confirm passing tests.

For each checkpoint record proposed/implemented/reviewed/tested/deployed separately, exact revisions and coverage.
An unresolved OS counterexample blocks the dependent custody claim; narrow the supported contract or choose another
mechanism instead of adding a timeout and declaring success. No model or reference above establishes actual Mac safety
without fixture evidence. No platform's results automatically qualify another platform. This addition ran no TLC,
Loom, process fixture, build, migration or production test. Implementation in new sessions must preserve the existing
proof inputs/receipts and separately authorized deployment boundary.

## 14. Compact model integration checkpoint

The reviewed Python model/checker sources and unparsed TLA+ files are now kept in
[`specs/native-execution`](../specs/native-execution/README.md). Its README records
the archive hash, replay commands and incomplete/unrun coverage. This integration
changes no runtime behavior. Sixty recorded tests and reduced graph checks are
not a completed combined model, TLC run or restart-recovery qualification.

Custody refinement additionally forbids SH/EX lock-mode conversion while live
custody exists, as well as explicit unlock through an alias. Use close-only
ownership; the model does not assume a conversion preserves exclusion atomically.
This assumption does not assert that every failed nonblocking conversion loses
its lock. Runtime syscall evidence and platform semantics remain separate gates.

## 15. Durable storage failure model and qualification requirements

Documentation clarification, 2026-11-17; no database or installed setting changed.
The proposed native store already requires WAL and `synchronous=FULL` (§5). This
is not discovery of a NORMAL-mode bug in an implemented native store: that store
does not yet exist. The existing Proofs `P/sqlite_store.py` separately sets FULL
on its connections; it is not the new native store and this edit does not alter it.

### Documented SQLite behavior versus the proposed contract

SQLite documents that committed WAL transactions with NORMAL can be lost after
an OS crash or power failure, although application crashes do not have that same
durability exposure. FULL adds a WAL sync for each committed transaction; EXTRA
adds nothing beyond FULL in WAL mode. On macOS, `fullfsync` defaults off; when
enabled it requests F_FULLFSYNC for supported sync operations. With fullfsync
on, checkpoint_fullfsync is redundant. Unknown PRAGMAs may be silently ignored.
Sources: [synchronous](https://sqlite.org/pragma.html#pragma_synchronous),
[fullfsync](https://sqlite.org/pragma.html#pragma_fullfsync),
[checkpoint_fullfsync](https://sqlite.org/pragma.html#pragma_checkpoint_fullfsync).

Proposed connection contract for the native store:

1. Before accepting durable mutations, configure and read back `journal_mode=WAL`,
   `synchronous=FULL` (2) and foreign keys ON on each relevant connection/schema.
   Do not assume connection-local settings persist through reopen or pooling.
2. On supported macOS SQLite/VFS builds, explicitly request/read back fullfsync ON.
   Do not substitute checkpoint_fullfsync alone for commit synchronization. Record
   SQLite version, VFS, effective values and filesystem assumptions in qualification.
3. A missing/mismatched required setting, failed commit or sync error refuses a
   new durable admission/GO acknowledgment. Return explicit storage failure; never
   quietly retry under NORMAL/OFF. Failed or lost commit acknowledgment can leave
   the transaction outcome uncertain: reconcile the original immutable identity,
   rather than assuming rollback or retrying its payload. Cleanup of existing scopes remains independent
   of a successful storage write. Configure storage during controlled initialization,
   not journal-mode transitions on a live operation path.
4. A PRAGMA value is configuration evidence, not proof that a particular device,
   filesystem, VFS or storage stack honored durable flush. Document platform limits
   and any weaker supported storage profile instead of claiming universal durability.

### Three distinct failure classes

- **Service/process crash:** OS and device remain running; kill/reopen tests can
  check journal replay, original IDs, unacknowledged results and unknown launch
  outcomes. Surviving native scopes still need custody reconciliation.
- **OS crash/reboot:** userspace and kernel state disappear. A process-kill test
  does not simulate dirty kernel buffers being lost. Old local process custody
  cannot survive a real reboot, but saved outcome/intent may still be incomplete.
- **Power loss:** additionally exercises device/cache persistence. Neither SIGKILL
  nor an ordinary VM process restart proves the physical storage flush contract.
  Never power-cycle Jacob's machine as a test. Any future destructive fault testing
  needs an isolated disposable environment and the appropriate authorization.

### Database, OS launch and external assets are separate durability domains

Preserve caller job/stage binding and native intent before dispatch. Commit launch
permission before GO, but do not claim commit and OS spawn are atomic: a committed
permission may never execute; an executed command may lose its completion report.
Missing result cannot authorize replay. Successful SQLite commit cannot preserve
an output file that was not separately flushed or stop a running process.

The §5 asset protocol must use an explicitly qualified platform file-sync helper:
write bounded temporary asset, flush file contents with the required supported
macOS full-sync policy where applicable, rename, establish directory-entry
persistence under the supported filesystem contract, then commit its database
reference. Qualify directory synchronization separately; do not assume a database
PRAGMA configures external files or that the same file-sync operation works for
all directory descriptors. Propagate failures before advertising a durable result.
The same analysis applies to caller provenance, scope manifests and epoch changes.
GC durably commits reference removal before deleting unreferenced assets and
serializes publication/deletion or rechecks under equivalent coordination so a
concurrent reference or live-scope pin cannot race deletion. It still protects
inputs needed by live scopes. A hash detects missing/corrupt bytes; it
cannot reconstruct them or establish that a command never ran.

### Required validation and honest reporting

- Assert effective PRAGMAs on fresh, reopened and pooled connections. Deliberately
  misconfigure NORMAL/fullfsync and ensure the admission check rejects the profile.
- Instrument a test VFS/storage adapter to verify sync ordering and inject write,
  flush, commit, rename and checkpoint failures. Label these simulated storage
  faults, not actual hardware power-loss experiments.
- Kill only disposable service processes at each binding/commit/GO/result/ACK cut;
  reopen and assert original identity retention, no reauthorization, honest unknowns
  and missing-asset errors. Test WAL recovery before and after checkpoint.
- Verify external-asset publish/GC ordering independently, including failures after
  file flush, rename, directory sync and database-reference commit. Cleanup cannot
  be trapped by the failure of its evidence store.
- Keep a coverage table separating process-kill tests, VFS fault simulations,
  OS-crash testing and physical power-loss qualification. Unrun categories stay
  unrun. No existing custody fixture or pure lifecycle model establishes these
  storage guarantees; all new native-store durability tests remain NOT RUN.
