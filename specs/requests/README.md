# Request model

`Requests.tla` specifies how the server keeps requests, the project lock and build process groups. It is the
specification for the request table and the lock in the server; it does not model Lake.

## What it covers

- **One row per request ID.** A client calls `run {id, intent}`. In one transaction the server inserts the row or reads
  the existing one. The same ID with another intent is refused. A row says `running` before anything launches, so a
  restarted server knows the request may have run; it records such a row as `interrupted` and never runs it again.
- **One lock with shared and exclusive holders.** A build holds it exclusively from the grant until its process group
  has exited. A check holds it shared while its worker loads imports, then elaborates with no lock. Waiters take the
  lock in arrival order: the server's lock is a fair queue (`tokio::sync::RwLock`). The kernel lock file is the part a
  crash cannot lose, because the build group keeps it.
- **One process group per build.** Lake and the compilers it starts. The lock and the request's end wait until every
  member has exited.
- **Warm workers.** An idle worker's imports are reused only if no build has ended since it loaded them. One counter
  records that.
- **Deadlines.** A request past its deadline is stopped, and its row records `timed_out` only after its processes are
  gone.

A build is one step that runs and exits. Lake's build graph, the content of results, and the client's transport are out
of scope.

## Properties

| Property | Rule | Mutant that TLC rejects |
| --- | --- | --- |
| `AtMostOneLaunch` | at most one launch per request ID | `NoInsertOrGet`, `RerunAfterRestart` |
| `LaunchRecorded` | a record of the launch exists before the launch | `SpawnBeforeMark` |
| `WriterExclusion` | at most one build group at a time, across server lives | `MemoryLockOnly` |
| `NoLoadDuringWrite` | no check loads imports while a build holds the lock | `CheckWithoutLock` |
| `NoEarlyLockRelease` | no lock release while a group member may live | `LockByLeader` |
| `WorkerBound` | no slot release while a worker may live; the pool stays bounded | `SlotOnKill` |
| `IntentConflictRefused` | the same ID with another intent is refused | `IgnoreIntent` |
| `FifoGrant` | within one server's life, the lock goes in arrival order | `UnfairLock` |
| `FreshImports` | a check uses imports loaded after every build that ended before it got the lock | `StaleReuse` |
| `ResultImmutable` | a stored result never changes | `LateResult` |
| `EveryRequestEnds` | every request with a row ends in a recorded state | `DropQueuedOnRestart` |

`Good.cfg` checks the safety properties with three requests, one crash and one worker. `Progress.cfg` checks
`EveryRequestEnds` under `FairSpec`. Each other configuration selects one mutant and names the one property it breaks.

## Assumptions

- The SDK's `WorkerCustody` model holds for the matched SDK release: the lock file stays held while any member of the
  group may live, and a worker's capacity returns only after it has exited (`WriterExclusion`, `NoEarlyCapacityReturn`
  there). This model takes those results as given.
- No process leaves its group, and a kill reaches every member. On Linux the SDK cannot yet confirm this for a late
  descendant.
- A SQLite transaction is atomic and survives a crash once committed.
- For progress: the server restarts after a crash, Lake and its children exit, and fair server steps run. Run calls,
  crashes and deadlines are never forced.

## Running

```sh
python3 specs/requests/check.py
python3 specs/requests/check.py --sdk ../lean-rs/specs/worker-custody
```

The runner finds `java` on `PATH`, in `JAVA`, or at Homebrew's `openjdk@21`, and `tla2tools.jar` in `TLA_TOOLS_JAR`
(default `~/.local/share/tlaplus/1.7.4/tla2tools.jar`). CI runs the second command against the SDK release in
`Cargo.lock`.

A pass means TLC found no counterexample within the bounds in the configuration files. It is not a proof about the
running server, the operating system, or SQLite.
