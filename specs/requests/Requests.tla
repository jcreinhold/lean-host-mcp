------------------------------ MODULE Requests ------------------------------
(***************************************************************************)
(* Requests, the project lock and build process groups in lean-host-mcp.  *)
(*                                                                         *)
(* The server keeps one table row per request ID. Requests take turns      *)
(* through the server's fair lock, in arrival order. A build takes the     *)
(* write turn, then the kernel lock file exclusively, and runs as one      *)
(* process group: Lake and the compilers it starts. A check takes a read   *)
(* turn and holds the lock file shared until it ends. A pool worker holds  *)
(* it shared from its start until it exits, busy or idle. The lock file is *)
(* the part a server crash cannot lose, and the only part that another     *)
(* writing process sees.                                                   *)
(*                                                                         *)
(* Lake's build graph is out of scope: a build is one step that runs and   *)
(* exits. The SDK's WorkerCustody model owns the custody of one process    *)
(* group; this model assumes its result. README.md states the mapping.     *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS
    Ids,          \* request IDs that clients may choose
    Intents,      \* tool and argument digests
    Builds,       \* the intents that are builds; the others are checks
    MaxWorkers,   \* size of the worker pool
    MaxRuns,      \* bound on run calls, to keep the model finite
    MaxCrashes,   \* bound on server crashes
    MaxExternal,  \* bound on builds by another process that uses the lock file
    Mode          \* "good", or one mutant from Mutants

\* Each mutant breaks one rule of the design. Its configuration names the
\* property that TLC must find violated.
Mutants == {
    "no_insert_or_get",       \* a repeated run schedules a new launch
    "spawn_before_mark",      \* Lake starts before its row says running
    "memory_lock_only",       \* the server waits on its own lock, not the file
    "lock_by_leader",         \* Lake's exit counts as the group's exit
    "check_without_lock",     \* a check loads imports without the lock
    "slot_on_kill",           \* a worker slot is freed when the kill is sent
    "late_result",            \* a deadline answers before Lake has stopped
    "ignore_intent",          \* a run with another intent gets the old row
    "unfair_lock",            \* any waiter may take a turn
    "no_guard",               \* the server holds no shared lock for a check
    "rerun_after_restart",    \* a restarted server runs an old row again
    "drop_queued_on_restart"  \* a restarted server forgets waiting requests
}

ASSUME /\ Builds \subseteq Intents
       /\ MaxWorkers \in Nat \ {0}
       /\ MaxRuns \in Nat
       /\ MaxCrashes \in Nat
       /\ MaxExternal \in Nat
       /\ Mode \in {"good"} \cup Mutants

None == "none"
External == "external"
Final == {"done", "timed_out", "interrupted"}
States == {"absent", "queued", "running"} \cup Final
\* A build: "turn" (write turn), "granted" (and the lock file), "marked",
\* "unmarked", "spawned", "stopping". A check: "read" (read turn), "ready"
\* (and the shared lock), "loading", "elab", "stopping".
Phases == {"none", "turn", "granted", "marked", "unmarked", "spawned",
           "stopping", "read", "ready", "loading", "elab"}
WorkerStates == {"none", "loading", "busy", "idle", "dying"}
Absent == [intent |-> None, state |-> "absent"]
NoWorker == [st |-> "none", srv |-> FALSE, job |-> None, ver |-> -1]

VARIABLES
    row,        \* store: the row for each ID; survives a crash
    launches,   \* history: launches per ID: Lake, or a job sent to a worker
    replies,    \* history: the answers to run calls
    runs,       \* history: run calls so far
    up,         \* the server is running
    boot,       \* server: "recover", "requeue" or "listen" after a start
    crashes,    \* server crashes so far
    queue,      \* server: turn waiters in arrival order
    nextNo,     \* server: the next arrival number in this server's life
    lastGrant,  \* server: the arrival number of the last waiter granted
    phase,      \* server: what the server is doing for each ID
    flock,      \* kernel: the exclusive holder of the lock file, or None
    guard,      \* kernel: the checks for which the server holds it shared
    leader,     \* process: a build's Lake process
    desc,       \* process: another member of a build's group
    helper,     \* process: a build's writer helper: "none", "live", "orphan"
    worker,     \* process: the worker each check started, by that check's ID
    ext,        \* process: another writer, "none", "wait" or "hold"
    extRuns,    \* history: builds by the other writer so far
    version,    \* artifacts: builds that have started writing so far
    lockedAt,   \* history: the version when each check took the shared lock
    usedAt      \* history: the version of the imports each check uses

vars == <<row, launches, replies, runs, up, boot, crashes, queue, nextNo,
          lastGrant, phase, flock, guard, leader, desc, helper, worker, ext,
          extRuns, version, lockedAt, usedAt>>

-----------------------------------------------------------------------------

IsBuild(id) == row[id].intent \in Builds
Live(b) == leader[b] \/ desc[b]
LiveBuilds == {b \in Ids : IsBuild(b) /\ Live(b)}

\* The workers this server started that have not exited.
Pool == {w \in Ids : worker[w].srv /\ worker[w].st # "none"}
\* The workers doing a check's job, including one left by a dead server.
Serving(c) == {w \in Ids : worker[w].st \in {"loading", "busy", "dying"}
                           /\ worker[w].job = c}
\* The slots this server counts as taken. A stopped worker keeps its slot
\* until it has exited.
Slots == {w \in Pool : worker[w].st # "dying" \/ Mode # "slot_on_kill"}

\* Every live worker holds the lock file shared, from its start until it
\* exits.
SharedHolders ==
    guard \cup IF Mode = "check_without_lock" THEN {}
               ELSE {w \in Ids : worker[w].st # "none"}
KernelFree == flock = None /\ SharedHolders = {}

\* The server's fair lock, as this server's own records show it.
WriteTurn == {b \in Ids : IsBuild(b) /\ phase[b] \in
               {"turn", "granted", "marked", "unmarked", "spawned", "stopping"}}
ReadTurns == {c \in Ids : ~IsBuild(c) /\ phase[c] \in
                {"read", "ready", "loading", "elab", "stopping"}}

Enqueue(id) ==
    /\ queue' = Append(queue, [id |-> id, no |-> nextNo])
    /\ nextNo' = nextNo + 1

Drop(s, k) == [i \in 1..(Len(s) - 1) |-> IF i < k THEN s[i] ELSE s[i + 1]]

Queued(id) == \E k \in 1..Len(queue) : queue[k].id = id

Grantable(k) == k \in 1..Len(queue) /\ (k = 1 \/ Mode = "unfair_lock")

GroupExited(b) == ~leader[b] /\ (~desc[b] \/ Mode = "lock_by_leader")

-----------------------------------------------------------------------------

Init ==
    /\ row = [id \in Ids |-> Absent]
    /\ launches = [id \in Ids |-> 0]
    /\ replies = {}
    /\ runs = 0
    /\ up = TRUE
    /\ boot = "listen"
    /\ crashes = 0
    /\ queue = <<>>
    /\ nextNo = 1
    /\ lastGrant = 0
    /\ phase = [id \in Ids |-> "none"]
    /\ flock = None
    /\ guard = {}
    /\ leader = [id \in Ids |-> FALSE]
    /\ desc = [id \in Ids |-> FALSE]
    /\ helper = [id \in Ids |-> "none"]
    /\ worker = [id \in Ids |-> NoWorker]
    /\ ext = "none"
    /\ extRuns = 0
    /\ version = 0
    /\ lockedAt = [id \in Ids |-> -1]
    /\ usedAt = [id \in Ids |-> -1]

\* A client calls run {id, intent}. In one transaction the server inserts the
\* row or reads the existing one, then answers. A new request waits for its
\* turn.
Run(id, i) ==
    /\ up
    /\ boot = "listen"
    /\ runs < MaxRuns
    /\ runs' = runs + 1
    /\ LET fresh == \/ row[id].state = "absent"
                    \/ /\ Mode = "no_insert_or_get"
                       /\ row[id].state \in Final
                       /\ row[id].intent = i
           same == row[id].intent = i \/ Mode = "ignore_intent"
       IN IF fresh
          THEN /\ row' = [row EXCEPT ![id] = [intent |-> i, state |-> "queued"]]
               /\ replies' = replies \cup {[id |-> id, intent |-> i, ok |-> TRUE]}
               /\ Enqueue(id)
          ELSE /\ replies' = replies \cup {[id |-> id, intent |-> i, ok |-> same]}
               /\ UNCHANGED <<row, queue, nextNo>>
    /\ UNCHANGED <<launches, up, boot, crashes, lastGrant, phase, flock, guard,
                   leader, desc, helper, worker, ext, extRuns, version,
                   lockedAt, usedAt>>

\* The head waiter, a build, takes the write turn once no turn is held.
GrantBuild(k) ==
    /\ up
    /\ boot # "recover"
    /\ Grantable(k)
    /\ LET b == queue[k].id IN
       /\ IsBuild(b)
       /\ row[b].state = "queued"
       /\ phase[b] = "none"
       /\ WriteTurn = {}
       /\ ReadTurns = {}
       /\ phase' = [phase EXCEPT ![b] = "turn"]
    /\ queue' = Drop(queue, k)
    /\ lastGrant' = queue[k].no
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, nextNo,
                   flock, guard, leader, desc, helper, worker, ext, extRuns,
                   version, lockedAt, usedAt>>

\* With the write turn, the build's helper takes the lock file exclusively.
\* It waits for every shared holder to go: idle workers retire on demand.
TakeLock(b) ==
    /\ up
    /\ phase[b] = "turn"
    /\ Mode = "memory_lock_only" \/ KernelFree
    /\ flock' = b
    /\ helper' = [helper EXCEPT ![b] = "live"]
    /\ phase' = [phase EXCEPT ![b] = "granted"]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, guard, leader, desc, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The row says running before Lake starts, so a restarted server knows the
\* build may have run.
MarkBuild(b) ==
    /\ up
    /\ Mode # "spawn_before_mark"
    /\ phase[b] = "granted"
    /\ row' = [row EXCEPT ![b].state = "running"]
    /\ phase' = [phase EXCEPT ![b] = "marked"]
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The SDK starts Lake as a new process group that inherits the lock file.
SpawnBuild(b) ==
    /\ up
    /\ phase[b] = IF Mode = "spawn_before_mark" THEN "granted" ELSE "marked"
    /\ leader' = [leader EXCEPT ![b] = TRUE]
    /\ desc' = [desc EXCEPT ![b] = TRUE]
    /\ launches' = [launches EXCEPT ![b] = @ + 1]
    /\ version' = version + 1
    /\ phase' = [phase EXCEPT ![b] =
                    IF Mode = "spawn_before_mark" THEN "unmarked" ELSE "spawned"]
    /\ UNCHANGED <<row, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, helper, worker, ext, extRuns,
                   lockedAt, usedAt>>

\* Only in spawn_before_mark: the row catches up after the launch.
MarkLate(b) ==
    /\ up
    /\ phase[b] = "unmarked"
    /\ row' = [row EXCEPT ![b].state = "running"]
    /\ phase' = [phase EXCEPT ![b] = "spawned"]
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* Group members exit on their own, whether or not the server is up.
LeaderExits(b) ==
    /\ leader[b]
    /\ leader' = [leader EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, desc, helper,
                   worker, ext, extRuns, version, lockedAt, usedAt>>

DescExits(b) ==
    /\ desc[b]
    /\ desc' = [desc EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, helper,
                   worker, ext, extRuns, version, lockedAt, usedAt>>

\* The lock file closes once the group is empty and the helper has gone: the
\* server closes the helper's session, or the SDK's custody has ended the
\* helper of a dead server.
ReleaseBuildLock(b) ==
    /\ flock = b
    /\ GroupExited(b)
    /\ \/ helper[b] = "none"
       \/ helper[b] = "live" /\ phase[b] \in {"spawned", "stopping"}
    /\ flock' = None
    /\ helper' = [helper EXCEPT ![b] = "none"]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, guard, leader, desc, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The SDK's custody ends the helper of a dead server.
ReapHelper(b) ==
    /\ helper[b] = "orphan"
    /\ helper' = [helper EXCEPT ![b] = "none"]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, desc,
                   worker, ext, extRuns, version, lockedAt, usedAt>>

\* The server stores Lake's exit code and gives up the write turn.
FinishBuild(b) ==
    /\ up
    /\ phase[b] = "spawned"
    /\ GroupExited(b)
    /\ flock # b
    /\ row' = [row EXCEPT ![b].state = "done"]
    /\ phase' = [phase EXCEPT ![b] = "none"]
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The deadline passes while Lake runs. The server asks the SDK to stop the
\* group and records the result only after the group is gone. The mutant
\* answers at once and leaves the exit handler to record the exit code.
ExpireBuild(b) ==
    /\ up
    /\ phase[b] = "spawned"
    /\ row[b].state = "running"
    /\ IF Mode = "late_result"
       THEN /\ row' = [row EXCEPT ![b].state = "timed_out"]
            /\ UNCHANGED phase
       ELSE /\ phase' = [phase EXCEPT ![b] = "stopping"]
            /\ UNCHANGED row
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The kill reaches every member of the group.
Kill(b) ==
    /\ IsBuild(b)
    /\ phase[b] = "stopping"
    /\ Live(b)
    /\ leader' = [leader EXCEPT ![b] = FALSE]
    /\ desc' = [desc EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

StoppedBuild(b) ==
    /\ up
    /\ IsBuild(b)
    /\ phase[b] = "stopping"
    /\ ~Live(b)
    /\ flock # b
    /\ row' = [row EXCEPT ![b].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![b] = "none"]
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* The head waiter, a check, takes a read turn once no build holds the
\* write turn. A build waiting ahead of it in the queue goes first.
GrantCheck(k) ==
    /\ up
    /\ boot # "recover"
    /\ Grantable(k)
    /\ LET c == queue[k].id IN
       /\ ~IsBuild(c)
       /\ row[c].state = "queued"
       /\ phase[c] = "none"
       /\ WriteTurn = {} \/ Mode = "check_without_lock"
       /\ phase' = [phase EXCEPT ![c] = "read"]
    /\ queue' = Drop(queue, k)
    /\ lastGrant' = queue[k].no
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, nextNo,
                   flock, guard, leader, desc, helper, worker, ext, extRuns,
                   version, lockedAt, usedAt>>

\* With the read turn, the server takes the lock file shared (the guard)
\* and records the row as running before any worker gets the job. The guard
\* lasts until the check ends, so no other writer runs between Lake's
\* setup-file and the worker's import load.
TakeGuard(c) ==
    /\ up
    /\ phase[c] = "read"
    /\ flock = None \/ Mode = "check_without_lock"
    /\ guard' = IF Mode \in {"no_guard", "check_without_lock"} THEN guard
                                                              ELSE guard \cup {c}
    /\ row' = [row EXCEPT ![c].state = "running"]
    /\ lockedAt' = [lockedAt EXCEPT ![c] = version]
    /\ phase' = [phase EXCEPT ![c] = "ready"]
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, leader, desc, helper, worker, ext,
                   extRuns, version, usedAt>>

\* An idle worker of this server takes the job with the imports it loaded.
Reuse(c, w) ==
    /\ up
    /\ phase[c] = "ready"
    /\ worker[w].srv
    /\ worker[w].st = "idle"
    /\ worker' = [worker EXCEPT ![w].st = "busy", ![w].job = c]
    /\ usedAt' = [usedAt EXCEPT ![c] = worker[w].ver]
    /\ launches' = [launches EXCEPT ![c] = @ + 1]
    /\ phase' = [phase EXCEPT ![c] = "elab"]
    /\ UNCHANGED <<row, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, ext, extRuns,
                   version, lockedAt>>

\* A new worker starts in a free slot, takes the lock file shared and loads
\* imports.
Spawn(c) ==
    /\ up
    /\ phase[c] = "ready"
    /\ worker[c].st = "none"
    /\ Cardinality(Slots) < MaxWorkers
    /\ flock = None \/ Mode = "check_without_lock"
    /\ worker' = [worker EXCEPT ![c] =
                     [st |-> "loading", srv |-> TRUE, job |-> c, ver |-> -1]]
    /\ launches' = [launches EXCEPT ![c] = @ + 1]
    /\ phase' = [phase EXCEPT ![c] = "loading"]
    /\ UNCHANGED <<row, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, ext, extRuns,
                   version, lockedAt, usedAt>>

LoadDone(c) ==
    /\ up
    /\ phase[c] = "loading"
    /\ worker[c].srv
    /\ worker[c].st = "loading"
    /\ worker' = [worker EXCEPT ![c].st = "busy", ![c].ver = version]
    /\ usedAt' = [usedAt EXCEPT ![c] = version]
    /\ phase' = [phase EXCEPT ![c] = "elab"]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, flock, guard, leader, desc, helper, ext,
                   extRuns, version, lockedAt>>

\* The check answers. Its worker goes idle and keeps its shared lock; the
\* server drops the guard and the read turn.
CheckDone(c) ==
    /\ up
    /\ phase[c] = "elab"
    /\ \E w \in Ids :
        /\ worker[w].srv
        /\ worker[w].st = "busy"
        /\ worker[w].job = c
        /\ worker' = [worker EXCEPT ![w].st = "idle", ![w].job = None]
    /\ row' = [row EXCEPT ![c].state = "done"]
    /\ phase' = [phase EXCEPT ![c] = "none"]
    /\ guard' = guard \ {c}
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, leader, desc, helper, ext, extRuns,
                   version, lockedAt, usedAt>>

\* The deadline passes after the check took the shared lock. The server
\* stops the worker doing its job, if there is one.
ExpireCheck(c) ==
    /\ up
    /\ ~IsBuild(c)
    /\ phase[c] \in {"ready", "loading", "elab"}
    /\ phase' = [phase EXCEPT ![c] = "stopping"]
    /\ worker' = [w \in Ids |->
                    IF worker[w].srv /\ worker[w].job = c
                       /\ worker[w].st \in {"loading", "busy"}
                    THEN [worker[w] EXCEPT !.st = "dying"]
                    ELSE worker[w]]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, flock, guard, leader, desc, helper, ext,
                   extRuns, version, lockedAt, usedAt>>

\* A stopped worker exits; only then does its slot return.
WorkerExits(w) ==
    /\ worker[w].st = "dying"
    /\ worker' = [worker EXCEPT ![w] = NoWorker]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, desc,
                   helper, ext, extRuns, version, lockedAt, usedAt>>

StoppedCheck(c) ==
    /\ up
    /\ ~IsBuild(c)
    /\ phase[c] = "stopping"
    /\ Serving(c) = {}
    /\ row' = [row EXCEPT ![c].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![c] = "none"]
    /\ guard' = guard \ {c}
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, queue, nextNo,
                   lastGrant, flock, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* An idle worker exits: its idle time ran out, a writer waits for the lock,
\* or a check needs its slot.
RetireIdle(w) ==
    /\ worker[w].srv
    /\ worker[w].st = "idle"
    /\ worker' = [worker EXCEPT ![w] = NoWorker]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, desc,
                   helper, ext, extRuns, version, lockedAt, usedAt>>

\* The deadline passes before the request took the lock file. It leaves the
\* queue or gives up its turn.
ExpireWaiting(id) ==
    /\ up
    /\ row[id].state = "queued"
    /\ phase[id] \in {"none", "turn", "read"}
    /\ row' = [row EXCEPT ![id].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![id] = "none"]
    /\ queue' = SelectSeq(queue, LAMBDA e : e.id # id)
    /\ UNCHANGED <<launches, replies, runs, up, boot, crashes, nextNo,
                   lastGrant, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

\* Another process that uses the lock file, such as a second server, builds.
ExtWant ==
    /\ ext = "none"
    /\ extRuns < MaxExternal
    /\ ext' = "wait"
    /\ extRuns' = extRuns + 1
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, desc,
                   helper, worker, version, lockedAt, usedAt>>

ExtTake ==
    /\ ext = "wait"
    /\ KernelFree
    /\ ext' = "hold"
    /\ flock' = External
    /\ version' = version + 1
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, guard, leader, desc, helper,
                   worker, extRuns, lockedAt, usedAt>>

ExtDone ==
    /\ ext = "hold"
    /\ ext' = "none"
    /\ flock' = None
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, guard, leader, desc, helper,
                   worker, extRuns, version, lockedAt, usedAt>>

\* The server dies. Its memory and its guards go; the store, the lock file,
\* build groups and workers remain.
Crash ==
    /\ up
    /\ crashes < MaxCrashes
    /\ up' = FALSE
    /\ crashes' = crashes + 1
    /\ queue' = <<>>
    /\ phase' = [id \in Ids |-> "none"]
    /\ guard' = {}
    /\ helper' = [b \in Ids |-> IF helper[b] = "live" THEN "orphan"
                                                   ELSE helper[b]]
    /\ worker' = [w \in Ids |-> [worker[w] EXCEPT !.srv = FALSE]]
    /\ UNCHANGED <<row, launches, replies, runs, boot, nextNo, lastGrant,
                   flock, leader, desc, ext, extRuns, version, lockedAt,
                   usedAt>>

\* The SDK's custody ends a worker whose server died.
ReapWorker(w) ==
    /\ ~worker[w].srv
    /\ worker[w].st # "none"
    /\ worker' = [worker EXCEPT ![w] = NoWorker]
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, queue,
                   nextNo, lastGrant, phase, flock, guard, leader, desc,
                   helper, ext, extRuns, version, lockedAt, usedAt>>

Restart ==
    /\ ~up
    /\ up' = TRUE
    /\ boot' = "recover"
    /\ nextNo' = 1
    /\ lastGrant' = 0
    /\ UNCHANGED <<row, launches, replies, runs, crashes, queue, phase, flock,
                   guard, leader, desc, helper, worker, ext, extRuns, version,
                   lockedAt, usedAt>>

\* Before it listens, a restarted server with rows left running takes the
\* write turn and the lock file exclusively. Getting the lock proves that no
\* Lake group or worker of the dead server remains. It records those rows as
\* interrupted and never runs them again.
Recover ==
    /\ up
    /\ boot = "recover"
    /\ LET running == {id \in Ids : row[id].state = "running"}
       IN /\ running = {} \/ KernelFree \/ Mode = "memory_lock_only"
          /\ row' = [id \in Ids |->
                       IF id \in running
                       THEN [row[id] EXCEPT !.state =
                               IF Mode = "rerun_after_restart" THEN "queued"
                                                               ELSE "interrupted"]
                       ELSE row[id]]
    /\ boot' = "requeue"
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, guard, leader, desc, helper,
                   worker, ext, extRuns, version, lockedAt, usedAt>>

\* Then every request that never got the lock waits for its turn again,
\* builds and checks alike.
Requeue(id) ==
    /\ up
    /\ boot = "requeue"
    /\ Mode # "drop_queued_on_restart"
    /\ row[id].state = "queued"
    /\ phase[id] = "none"
    /\ ~Queued(id)
    /\ Enqueue(id)
    /\ UNCHANGED <<row, launches, replies, runs, up, boot, crashes, lastGrant,
                   phase, flock, guard, leader, desc, helper, worker, ext,
                   extRuns, version, lockedAt, usedAt>>

Listen ==
    /\ up
    /\ boot = "requeue"
    /\ \/ Mode = "drop_queued_on_restart"
       \/ \A id \in Ids : (row[id].state = "queued" /\ phase[id] = "none")
                              => Queued(id)
    /\ boot' = "listen"
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, guard, leader, desc, helper,
                   worker, ext, extRuns, version, lockedAt, usedAt>>

Next ==
    \/ \E id \in Ids, i \in Intents : Run(id, i)
    \/ \E k \in 1..Len(queue) : GrantBuild(k) \/ GrantCheck(k)
    \/ ExtWant \/ ExtTake \/ ExtDone
    \/ Crash \/ Restart \/ Recover \/ Listen
    \/ \E id \in Ids :
        \/ TakeLock(id) \/ MarkBuild(id) \/ SpawnBuild(id) \/ MarkLate(id)
        \/ LeaderExits(id) \/ DescExits(id) \/ ReleaseBuildLock(id)
        \/ FinishBuild(id) \/ ExpireBuild(id) \/ Kill(id) \/ StoppedBuild(id)
        \/ TakeGuard(id) \/ Spawn(id) \/ LoadDone(id) \/ CheckDone(id)
        \/ ExpireCheck(id) \/ WorkerExits(id) \/ StoppedCheck(id)
        \/ RetireIdle(id) \/ ExpireWaiting(id) \/ ReapWorker(id)
        \/ Requeue(id) \/ ReapHelper(id)
        \/ \E w \in Ids : Reuse(id, w)

Spec == Init /\ [][Next]_vars

\* Progress assumes that the server restarts, that Lake, its children and
\* stopped workers exit, that idle workers retire, and that the other writer
\* finishes. Run calls, crashes, deadlines and the other writer's builds are
\* never forced.
Fairness ==
    /\ WF_vars(Restart) /\ WF_vars(Recover) /\ WF_vars(Listen)
    /\ WF_vars(\E k \in 1..Len(queue) : GrantBuild(k) \/ GrantCheck(k))
    /\ WF_vars(ExtTake) /\ WF_vars(ExtDone)
    /\ \A id \in Ids :
        /\ WF_vars(TakeLock(id)) /\ WF_vars(MarkBuild(id))
        /\ WF_vars(SpawnBuild(id)) /\ WF_vars(MarkLate(id))
        /\ WF_vars(LeaderExits(id)) /\ WF_vars(DescExits(id))
        /\ WF_vars(ReleaseBuildLock(id)) /\ WF_vars(FinishBuild(id))
        /\ WF_vars(Kill(id)) /\ WF_vars(StoppedBuild(id))
        /\ WF_vars(TakeGuard(id))
        /\ WF_vars(Spawn(id) \/ \E w \in Ids : Reuse(id, w))
        /\ WF_vars(LoadDone(id)) /\ WF_vars(CheckDone(id))
        /\ WF_vars(WorkerExits(id)) /\ WF_vars(StoppedCheck(id))
        /\ WF_vars(RetireIdle(id)) /\ WF_vars(ReapWorker(id))
        /\ WF_vars(Requeue(id)) /\ WF_vars(ReapHelper(id))

FairSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Properties.                                                             *)

TypeOK ==
    /\ row \in [Ids -> [intent : Intents \cup {None}, state : States]]
    /\ launches \in [Ids -> Nat]
    /\ replies \subseteq [id : Ids, intent : Intents, ok : BOOLEAN]
    /\ runs \in 0..MaxRuns
    /\ up \in BOOLEAN
    /\ boot \in {"recover", "requeue", "listen"}
    /\ crashes \in 0..MaxCrashes
    /\ queue \in Seq([id : Ids, no : Nat])
    /\ nextNo \in Nat \ {0}
    /\ lastGrant \in Nat
    /\ phase \in [Ids -> Phases]
    /\ flock \in Ids \cup {None, External}
    /\ guard \subseteq Ids
    /\ leader \in [Ids -> BOOLEAN]
    /\ desc \in [Ids -> BOOLEAN]
    /\ helper \in [Ids -> {"none", "live", "orphan"}]
    /\ worker \in [Ids -> [st : WorkerStates, srv : BOOLEAN,
                           job : Ids \cup {None}, ver : Int]]
    /\ ext \in {"none", "wait", "hold"}
    /\ extRuns \in 0..MaxExternal
    /\ version \in Nat
    /\ lockedAt \in [Ids -> Int]
    /\ usedAt \in [Ids -> Int]

\* At most one launch per request ID.
AtMostOneLaunch == \A id \in Ids : launches[id] <= 1

\* A record of the launch exists before the launch: while Lake runs for a
\* request, or a worker does its job, its row says running.
LaunchRecorded ==
    \A id \in Ids : (Live(id) \/ Serving(id) # {}) => row[id].state = "running"

\* At most one writer at a time, across server lives and processes.
WriterExclusion ==
    Cardinality(LiveBuilds) + (IF ext = "hold" THEN 1 ELSE 0) <= 1

\* No worker loads imports while a writer runs.
NoLoadDuringWrite ==
    (\E w \in Ids : worker[w].st = "loading")
        => (LiveBuilds = {} /\ ext # "hold")

\* No lock release while a group member may live.
NoEarlyLockRelease == \A b \in LiveBuilds : flock = b

\* No slot returns while its worker may live, so this server never runs
\* more workers than its pool allows. A dead server's workers only exit.
WorkerBound == Cardinality(Pool) <= MaxWorkers

\* The same ID with a different intent is refused.
IntentConflictRefused ==
    \A r \in replies : r.ok => row[r.id].intent = r.intent

\* Within one server's life, turns go in arrival order: every waiter
\* arrived after the last waiter granted.
FifoGrant == \A k \in 1..Len(queue) : queue[k].no > lastGrant

\* A check uses the imports of the artifacts it saw when it took the shared
\* lock: no build started between its setup-file and its use of the imports.
FreshImports == \A c \in Ids : phase[c] = "elab" => usedAt[c] = lockedAt[c]

\* A stored result never changes.
ResultImmutable ==
    [][\A id \in Ids : row[id].state \in Final => row'[id] = row[id]]_vars

\* Every request that has a row ends in a recorded state.
EveryRequestEnds ==
    \A id \in Ids : (row[id].state # "absent") ~> (row[id].state \in Final)

=============================================================================
