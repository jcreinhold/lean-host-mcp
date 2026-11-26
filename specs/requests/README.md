# Request model

`Requests.tla` specifies how the server keeps requests, the project lock and build process groups. It is the
specification for the request table and the lock in the server; it does not model Lake.

## What it covers

- **One row per request ID.** A client calls `run {id, intent}`. In one transaction the server inserts the row or reads
  the existing one. The same ID with another intent is refused. A row says `running` before anything launches, so a
  restarted server knows the request may have run; it records such a row as `interrupted` and never runs it again.
- **Turns, then the lock file.** Every request waits for a turn in the server's fair lock (`tokio::sync::RwLock`), in
  arrival order. A build takes the write turn, then the kernel lock file exclusively. A check takes a read turn, then
  the lock file shared (the server's guard), and keeps both until it ends. The lock file is the part a crash cannot
  lose, and the only part another writing process sees.
- **One process group per build.** Lake and the compilers it starts. The lock and the request's end wait until every
  member has exited.
- **Workers.** A pool worker holds the lock file shared from its start until it exits, busy or idle, so a build waits
  for idle workers to retire. A stopped worker keeps its slot until it has exited.
- **Another writer.** A second process that uses the lock file, such as another server, can build at any time the lock
  file is free.
- **Restart.** A restarted server with rows left running takes the lock file exclusively before it listens, records
  those rows as `interrupted`, and queues every row that never got the lock again.
- **Deadlines.** A request past its deadline is stopped, and its row records `timed_out` only after its processes are
  gone.

A build is one step that runs and exits. Lake's build graph, the content of results, and the client's transport are out
of scope.

## Properties

| Property | Rule | Mutant that TLC rejects |
| --- | --- | --- |
| `AtMostOneLaunch` | at most one launch per request ID | `NoInsertOrGet`, `RerunAfterRestart` |
| `LaunchRecorded` | a record of the launch exists before the launch | `SpawnBeforeMark` |
| `WriterExclusion` | at most one writer at a time, across server lives and processes | `MemoryLockOnly` |
| `NoLoadDuringWrite` | no worker loads imports while a writer runs | `CheckWithoutLock` |
| `NoEarlyLockRelease` | no lock release while a group member may live | `LockByLeader` |
| `WorkerBound` | no slot returns while its worker may live; the pool stays bounded | `SlotOnKill` |
| `IntentConflictRefused` | the same ID with another intent is refused | `IgnoreIntent` |
| `FifoGrant` | within one server's life, turns go in arrival order | `UnfairLock` |
| `FreshImports` | no build starts between a check's setup-file and its use of the imports | `NoGuard` |
| `ResultImmutable` | a stored result never changes | `LateResult` |
| `EveryRequestEnds` | every request with a row ends in a recorded state | `DropQueuedOnRestart` |

`Good.cfg` checks the safety properties with three requests, one crash, one worker and one other writer. `Progress.cfg`
checks `EveryRequestEnds` under `FairSpec` with the same writer. Each other configuration selects one mutant and names
the one property it breaks.

## Assumptions

- The SDK's `WorkerCustody` model holds for the matched SDK release: the lock file stays held while any member of the
  group may live, and a worker's capacity returns only after it has exited (`WriterExclusion`, `NoEarlyCapacityReturn`
  there). This model takes those results as given.
- A worker holds the lock file shared for its whole life, as SDK 0.10.17 does. Warm reuse is safe because of that lock,
  so the model has no build counter; the server keeps one only to report rebuilt imports and as a backstop. An SDK that
  released the lock once imports load would need a model change first.
- No process leaves its group, and a kill reaches every member. On Linux the SDK cannot yet confirm this for a late
  descendant.
- Every writer uses the lock file. A `lake build` run without it is out of scope.
- A SQLite transaction is atomic and survives a crash once committed.
- For progress: the server restarts after a crash; Lake, its children and stopped workers exit; idle workers retire; the
  other writer finishes; fair server steps run. Run calls, crashes, deadlines and the other writer's builds are never
  forced.

The reaper's proxy turn for another writer's demand is left out. It keeps that writer from waiting behind an endless
stream of checks, which a bounded model cannot show.

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
