------------------------------ MODULE Requests ------------------------------
(***************************************************************************)
(* Requests, the project lock and build process groups in lean-host-mcp.  *)
(*                                                                         *)
(* The server keeps one table row per request ID. A build holds the        *)
(* project lock exclusively and runs as one process group: Lake and the    *)
(* compilers it starts. A check holds the lock shared while its worker     *)
(* loads imports, then elaborates with no lock. Waiters take the lock in   *)
(* arrival order through the server's fair lock. The kernel lock file is   *)
(* the part a server crash cannot lose: the build group keeps it.          *)
(*                                                                         *)
(* Lake's build graph is out of scope: a build is one step that runs and   *)
(* exits. The SDK's WorkerCustody model owns the custody of one process    *)
(* group; this model assumes its result. README.md states the mapping.     *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS
    Ids,         \* request IDs that clients may choose
    Intents,     \* tool and argument digests
    Builds,      \* the intents that are builds; the others are checks
    MaxWorkers,  \* size of the worker pool
    MaxRuns,     \* bound on run calls, to keep the model finite
    MaxCrashes,  \* bound on server crashes
    Mode         \* "good", or one mutant from Mutants

\* Each mutant breaks one rule of the design. Its configuration names the
\* property that TLC must find violated.
Mutants == {
    "no_insert_or_get",       \* a repeated run schedules a new launch
    "spawn_before_mark",      \* Lake starts before its row says running
    "memory_lock_only",       \* builds wait only on the server's own lock
    "lock_by_leader",         \* Lake's exit counts as the group's exit
    "check_without_lock",     \* a check loads imports without the lock
    "slot_on_kill",           \* a worker slot is freed when the kill is sent
    "late_result",            \* a deadline answers before Lake has stopped
    "ignore_intent",          \* a run with another intent gets the old row
    "unfair_lock",            \* any waiter may take the lock
    "stale_reuse",            \* a warm worker is reused after a build ended
    "rerun_after_restart",    \* a restarted server runs an old row again
    "drop_queued_on_restart"  \* a restarted server forgets waiting builds
}

ASSUME /\ Builds \subseteq Intents
       /\ MaxWorkers \in Nat \ {0}
       /\ MaxRuns \in Nat
       /\ MaxCrashes \in Nat
       /\ Mode \in {"good"} \cup Mutants

None == "none"
Final == {"done", "timed_out", "interrupted"}
States == {"absent", "queued", "running"} \cup Final
Phases == {"none", "slot", "granted", "marked", "unmarked", "spawned",
           "stopping", "loading", "elab"}
Absent == [intent |-> None, state |-> "absent"]

VARIABLES
    row,        \* store: the row for each ID; survives a crash
    launches,   \* history: processes launched for each ID
    replies,    \* history: the answers to run calls
    runs,       \* history: run calls so far
    up,         \* the server is running
    crashes,    \* server crashes so far
    queue,      \* server: lock waiters in arrival order
    nextNo,     \* server: the next arrival number in this server's life
    lastGrant,  \* server: the arrival number of the last waiter granted
    phase,      \* server: what the server is doing for each ID
    flock,      \* kernel: the exclusive holder of the lock file, or None
    flockSh,    \* kernel: the shared holders of the lock file
    leader,     \* process: a build's Lake process, or a check's worker
    desc,       \* process: another member of a build's group
    loading,    \* process: the checks whose workers are loading imports
    finished,   \* server: builds ended so far; the warm-reuse counter
    warm,       \* server: the counter when the idle worker loaded, or -1
    grantedAt,  \* server: the counter when each check got the lock
    loadedAt    \* server: the counter of the imports each check uses

vars == <<row, launches, replies, runs, up, crashes, queue, nextNo,
          lastGrant, phase, flock, flockSh, leader, desc, loading, finished,
          warm, grantedAt, loadedAt>>

-----------------------------------------------------------------------------

IsBuild(id) == row[id].intent \in Builds
Live(id) == leader[id] \/ desc[id]
LiveBuilds == {b \in Ids : IsBuild(b) /\ Live(b)}
Workers == {c \in Ids : ~IsBuild(c) /\ leader[c]}

\* The server's fair lock, as this server's own records show it.
ServerExclusive == {b \in Ids : IsBuild(b) /\ phase[b] \in
                       {"granted", "marked", "unmarked", "spawned", "stopping"}}
ServerShared == {c \in Ids : phase[c] = "loading"}

\* Worker slots the server counts as taken. A live worker keeps its slot
\* until it has exited, including one left by a dead server.
SlotHolders ==
    {c \in Ids : phase[c] = "slot"}
    \cup {c \in Workers : Mode # "slot_on_kill" \/ phase[c] # "stopping"}

Enqueue(id) ==
    /\ queue' = Append(queue, [id |-> id, no |-> nextNo])
    /\ nextNo' = nextNo + 1

Drop(s, k) == [i \in 1..(Len(s) - 1) |-> IF i < k THEN s[i] ELSE s[i + 1]]

Grantable(k) == k \in 1..Len(queue) /\ (k = 1 \/ Mode = "unfair_lock")

GroupExited(b) == ~leader[b] /\ (~desc[b] \/ Mode = "lock_by_leader")

-----------------------------------------------------------------------------

Init ==
    /\ row = [id \in Ids |-> Absent]
    /\ launches = [id \in Ids |-> 0]
    /\ replies = {}
    /\ runs = 0
    /\ up = TRUE
    /\ crashes = 0
    /\ queue = <<>>
    /\ nextNo = 1
    /\ lastGrant = 0
    /\ phase = [id \in Ids |-> "none"]
    /\ flock = None
    /\ flockSh = {}
    /\ leader = [id \in Ids |-> FALSE]
    /\ desc = [id \in Ids |-> FALSE]
    /\ loading = {}
    /\ finished = 0
    /\ warm = -1
    /\ grantedAt = [id \in Ids |-> -1]
    /\ loadedAt = [id \in Ids |-> -1]

\* A client calls run {id, intent}. In one transaction the server inserts the
\* row or reads the existing one, then answers. A build waits for the lock at
\* once; a check first takes a worker slot.
Run(id, i) ==
    /\ up
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
               /\ IF i \in Builds THEN Enqueue(id)
                                  ELSE UNCHANGED <<queue, nextNo>>
          ELSE /\ replies' = replies \cup {[id |-> id, intent |-> i, ok |-> same]}
               /\ UNCHANGED <<row, queue, nextNo>>
    /\ UNCHANGED <<launches, up, crashes, lastGrant, phase, flock, flockSh,
                   leader, desc, loading, finished, warm, grantedAt, loadedAt>>

TakeSlot(c) ==
    /\ up
    /\ ~IsBuild(c)
    /\ row[c].state = "queued"
    /\ phase[c] = "none"
    /\ Cardinality(SlotHolders) < MaxWorkers
    /\ phase' = [phase EXCEPT ![c] = "slot"]
    /\ Enqueue(c)
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, lastGrant, flock,
                   flockSh, leader, desc, loading, finished, warm, grantedAt,
                   loadedAt>>

\* The head waiter takes the lock once no build holds it, in this server's
\* records or in the kernel.
GrantBuild(k) ==
    /\ up
    /\ Grantable(k)
    /\ LET b == queue[k].id IN
       /\ IsBuild(b)
       /\ row[b].state = "queued"
       /\ phase[b] = "none"
       /\ ServerExclusive = {}
       /\ ServerShared = {}
       /\ Mode = "memory_lock_only" \/ (flock = None /\ flockSh = {})
       /\ phase' = [phase EXCEPT ![b] = "granted"]
    /\ queue' = Drop(queue, k)
    /\ lastGrant' = queue[k].no
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, nextNo, flock,
                   flockSh, leader, desc, loading, finished, warm, grantedAt,
                   loadedAt>>

\* The row says running before Lake starts, so a restarted server knows the
\* build may have run.
MarkBuild(b) ==
    /\ up
    /\ Mode # "spawn_before_mark"
    /\ phase[b] = "granted"
    /\ row' = [row EXCEPT ![b].state = "running"]
    /\ phase' = [phase EXCEPT ![b] = "marked"]
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

\* The SDK starts Lake as a new process group that inherits the lock file.
SpawnBuild(b) ==
    /\ up
    /\ phase[b] = IF Mode = "spawn_before_mark" THEN "granted" ELSE "marked"
    /\ leader' = [leader EXCEPT ![b] = TRUE]
    /\ desc' = [desc EXCEPT ![b] = TRUE]
    /\ flock' = b
    /\ launches' = [launches EXCEPT ![b] = @ + 1]
    /\ phase' = [phase EXCEPT ![b] =
                    IF Mode = "spawn_before_mark" THEN "unmarked" ELSE "spawned"]
    /\ UNCHANGED <<row, replies, runs, up, crashes, queue, nextNo, lastGrant,
                   flockSh, loading, finished, warm, grantedAt, loadedAt>>

\* Only in spawn_before_mark: the row catches up after the launch.
MarkLate(b) ==
    /\ up
    /\ phase[b] = "unmarked"
    /\ row' = [row EXCEPT ![b].state = "running"]
    /\ phase' = [phase EXCEPT ![b] = "spawned"]
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

\* Group members exit on their own, whether or not the server is up.
LeaderExits(b) ==
    /\ IsBuild(b)
    /\ leader[b]
    /\ leader' = [leader EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, flockSh, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

DescExits(b) ==
    /\ desc[b]
    /\ desc' = [desc EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, flockSh, leader, loading, finished,
                   warm, grantedAt, loadedAt>>

\* The SDK's group owner closes the lock file once the group is empty. It
\* does so whether or not the server is still up.
ReleaseBuildLock(b) ==
    /\ flock = b
    /\ GroupExited(b)
    /\ flock' = None
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

\* The server stores Lake's exit code and lets the next waiter in.
FinishBuild(b) ==
    /\ up
    /\ phase[b] = "spawned"
    /\ GroupExited(b)
    /\ flock # b
    /\ row' = [row EXCEPT ![b].state = "done"]
    /\ phase' = [phase EXCEPT ![b] = "none"]
    /\ finished' = finished + 1
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, warm,
                   grantedAt, loadedAt>>

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
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

\* The kill reaches every member of the group, or the check's worker.
Kill(id) ==
    /\ phase[id] = "stopping"
    /\ Live(id)
    /\ leader' = [leader EXCEPT ![id] = FALSE]
    /\ desc' = [desc EXCEPT ![id] = FALSE]
    /\ loading' = loading \ {id}
    /\ flockSh' = flockSh \ {id}
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, finished, warm, grantedAt, loadedAt>>

StoppedBuild(b) ==
    /\ up
    /\ IsBuild(b)
    /\ phase[b] = "stopping"
    /\ ~Live(b)
    /\ flock # b
    /\ row' = [row EXCEPT ![b].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![b] = "none"]
    /\ finished' = finished + 1
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, warm,
                   grantedAt, loadedAt>>

\* The head waiter, a check holding a slot, takes the lock shared once no
\* build holds it. Its worker then loads imports.
GrantCheck(k) ==
    /\ up
    /\ Grantable(k)
    /\ LET c == queue[k].id IN
       /\ ~IsBuild(c)
       /\ row[c].state = "queued"
       /\ phase[c] = "slot"
       /\ Mode = "check_without_lock" \/ (ServerExclusive = {} /\ flock = None)
       /\ row' = [row EXCEPT ![c].state = "running"]
       /\ phase' = [phase EXCEPT ![c] = "loading"]
       /\ leader' = [leader EXCEPT ![c] = TRUE]
       /\ loading' = loading \cup {c}
       /\ flockSh' = IF Mode = "check_without_lock" THEN flockSh
                                                    ELSE flockSh \cup {c}
       /\ launches' = [launches EXCEPT ![c] = @ + 1]
       /\ grantedAt' = [grantedAt EXCEPT ![c] = finished]
    /\ queue' = Drop(queue, k)
    /\ lastGrant' = queue[k].no
    /\ UNCHANGED <<replies, runs, up, crashes, nextNo, flock, desc, finished,
                   warm, loadedAt>>

\* The imports are loaded: the check gives up the lock and elaborates. The
\* idle worker's imports are reused only if no build has ended since it
\* loaded them.
LoadDone(c) ==
    /\ up
    /\ phase[c] = "loading"
    /\ LET reuse == warm >= 0 /\ (warm = finished \/ Mode = "stale_reuse")
       IN /\ loadedAt' = [loadedAt EXCEPT ![c] = IF reuse THEN warm ELSE finished]
          /\ warm' = IF reuse THEN -1 ELSE warm
    /\ loading' = loading \ {c}
    /\ flockSh' = flockSh \ {c}
    /\ phase' = [phase EXCEPT ![c] = "elab"]
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, leader, desc, finished, grantedAt>>

CheckDone(c) ==
    /\ up
    /\ phase[c] = "elab"
    /\ row' = [row EXCEPT ![c].state = "done"]
    /\ phase' = [phase EXCEPT ![c] = "none"]
    /\ leader' = [leader EXCEPT ![c] = FALSE]
    /\ warm' = loadedAt[c]
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, desc, loading, finished,
                   grantedAt, loadedAt>>

ExpireCheck(c) ==
    /\ up
    /\ ~IsBuild(c)
    /\ phase[c] \in {"loading", "elab"}
    /\ phase' = [phase EXCEPT ![c] = "stopping"]
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

StoppedCheck(c) ==
    /\ up
    /\ ~IsBuild(c)
    /\ phase[c] = "stopping"
    /\ ~leader[c]
    /\ row' = [row EXCEPT ![c].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![c] = "none"]
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, flock, flockSh, leader, desc, loading, finished,
                   warm, grantedAt, loadedAt>>

\* The deadline passes before the request got the lock.
ExpireWaiting(id) ==
    /\ up
    /\ row[id].state = "queued"
    /\ phase[id] \in {"none", "slot"}
    /\ row' = [row EXCEPT ![id].state = "timed_out"]
    /\ phase' = [phase EXCEPT ![id] = "none"]
    /\ queue' = SelectSeq(queue, LAMBDA e : e.id # id)
    /\ UNCHANGED <<launches, replies, runs, up, crashes, nextNo, lastGrant,
                   flock, flockSh, leader, desc, loading, finished, warm,
                   grantedAt, loadedAt>>

\* The server dies. Its memory goes; the store, the kernel lock and the
\* processes it started remain.
Crash ==
    /\ up
    /\ crashes < MaxCrashes
    /\ up' = FALSE
    /\ crashes' = crashes + 1
    /\ queue' = <<>>
    /\ phase' = [id \in Ids |-> "none"]
    /\ warm' = -1
    /\ UNCHANGED <<row, launches, replies, runs, nextNo, lastGrant, flock,
                   flockSh, leader, desc, loading, finished, grantedAt,
                   loadedAt>>

\* The SDK's custody ends a check worker whose server died.
ReapWorker(c) ==
    /\ ~IsBuild(c)
    /\ leader[c]
    /\ phase[c] = "none"
    /\ leader' = [leader EXCEPT ![c] = FALSE]
    /\ loading' = loading \ {c}
    /\ flockSh' = flockSh \ {c}
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, desc, finished, warm, grantedAt,
                   loadedAt>>

Restart ==
    /\ ~up
    /\ up' = TRUE
    /\ nextNo' = 1
    /\ lastGrant' = 0
    /\ UNCHANGED <<row, launches, replies, runs, crashes, queue, phase, flock,
                   flockSh, leader, desc, loading, finished, warm, grantedAt,
                   loadedAt>>

\* After a restart, a build that never launched waits for the lock again.
Requeue(b) ==
    /\ up
    /\ Mode # "drop_queued_on_restart"
    /\ IsBuild(b)
    /\ row[b].state = "queued"
    /\ phase[b] = "none"
    /\ \A k \in 1..Len(queue) : queue[k].id # b
    /\ Enqueue(b)
    /\ UNCHANGED <<row, launches, replies, runs, up, crashes, lastGrant, phase,
                   flock, flockSh, leader, desc, loading, finished, warm,
                   grantedAt, loadedAt>>

\* After a restart, a row left running may have launched. Once its processes
\* and locks are gone, the server records it as interrupted; it never runs it
\* again. The reuse counter stays: the crash emptied the worker pool, and no
\* worker can load imports until the old build has released the lock.
Recover(id) ==
    /\ up
    /\ row[id].state = "running"
    /\ phase[id] = "none"
    /\ ~Live(id)
    /\ flock # id
    /\ id \notin flockSh
    /\ row' = [row EXCEPT ![id].state =
                  IF Mode = "rerun_after_restart" THEN "queued" ELSE "interrupted"]
    /\ UNCHANGED <<launches, replies, runs, up, crashes, queue, nextNo,
                   lastGrant, phase, flock, flockSh, leader, desc, loading,
                   finished, warm, grantedAt, loadedAt>>

Next ==
    \/ \E id \in Ids, i \in Intents : Run(id, i)
    \/ \E k \in 1..Len(queue) : GrantBuild(k) \/ GrantCheck(k)
    \/ Crash
    \/ Restart
    \/ \E id \in Ids :
        \/ TakeSlot(id) \/ MarkBuild(id) \/ SpawnBuild(id) \/ MarkLate(id)
        \/ LeaderExits(id) \/ DescExits(id) \/ ReleaseBuildLock(id)
        \/ FinishBuild(id) \/ ExpireBuild(id) \/ Kill(id) \/ StoppedBuild(id)
        \/ LoadDone(id) \/ CheckDone(id) \/ ExpireCheck(id) \/ StoppedCheck(id)
        \/ ExpireWaiting(id) \/ ReapWorker(id) \/ Requeue(id) \/ Recover(id)

Spec == Init /\ [][Next]_vars

\* Progress assumes that the server restarts, that Lake and its children
\* exit, and that the kill reaches every member of a group. Run calls, crashes
\* and deadlines are never forced.
Fairness ==
    /\ WF_vars(Restart)
    /\ WF_vars(\E k \in 1..Len(queue) : GrantBuild(k) \/ GrantCheck(k))
    /\ \A id \in Ids :
        /\ WF_vars(TakeSlot(id)) /\ WF_vars(MarkBuild(id))
        /\ WF_vars(SpawnBuild(id)) /\ WF_vars(MarkLate(id))
        /\ WF_vars(LeaderExits(id)) /\ WF_vars(DescExits(id))
        /\ WF_vars(ReleaseBuildLock(id)) /\ WF_vars(FinishBuild(id))
        /\ WF_vars(Kill(id)) /\ WF_vars(StoppedBuild(id))
        /\ WF_vars(LoadDone(id)) /\ WF_vars(CheckDone(id))
        /\ WF_vars(StoppedCheck(id)) /\ WF_vars(ReapWorker(id))
        /\ WF_vars(Requeue(id)) /\ WF_vars(Recover(id))

FairSpec == Spec /\ Fairness

-----------------------------------------------------------------------------
(* Properties.                                                             *)

TypeOK ==
    /\ row \in [Ids -> [intent : Intents \cup {None}, state : States]]
    /\ launches \in [Ids -> Nat]
    /\ replies \subseteq [id : Ids, intent : Intents, ok : BOOLEAN]
    /\ runs \in 0..MaxRuns
    /\ up \in BOOLEAN
    /\ crashes \in 0..MaxCrashes
    /\ queue \in Seq([id : Ids, no : Nat])
    /\ nextNo \in Nat \ {0}
    /\ lastGrant \in Nat
    /\ phase \in [Ids -> Phases]
    /\ flock \in Ids \cup {None}
    /\ flockSh \subseteq Ids
    /\ leader \in [Ids -> BOOLEAN]
    /\ desc \in [Ids -> BOOLEAN]
    /\ loading \subseteq Ids
    /\ finished \in Nat
    /\ warm \in Int
    /\ grantedAt \in [Ids -> Int]
    /\ loadedAt \in [Ids -> Int]

\* At most one launch per request ID.
AtMostOneLaunch == \A id \in Ids : launches[id] <= 1

\* A record of the launch exists before the launch: while any process of a
\* request lives, its row says running.
LaunchRecorded == \A id \in Ids : Live(id) => row[id].state = "running"

\* At most one writer per lock: one build group at a time, across server
\* lives.
WriterExclusion == Cardinality(LiveBuilds) <= 1

\* No check loads imports while a writer holds the lock.
NoLoadDuringWrite == loading # {} => LiveBuilds = {}

\* No lock release while a group member may live.
NoEarlyLockRelease == \A b \in LiveBuilds : flock = b

\* No slot release while a worker may live, so the pool stays bounded.
WorkerBound == Cardinality(Workers) <= MaxWorkers

\* The same ID with a different intent is refused.
IntentConflictRefused ==
    \A r \in replies : r.ok => row[r.id].intent = r.intent

\* Within one server's life the lock is granted in arrival order: every
\* waiter arrived after the last waiter granted.
FifoGrant == \A k \in 1..Len(queue) : queue[k].no > lastGrant

\* A check uses imports loaded after every build that ended before it got
\* the lock.
FreshImports ==
    \A c \in Ids : phase[c] = "elab" => loadedAt[c] = grantedAt[c]

\* A stored result never changes.
ResultImmutable ==
    [][\A id \in Ids : row[id].state \in Final => row'[id] = row[id]]_vars

\* Every request that has a row ends in a recorded state.
EveryRequestEnds ==
    \A id \in Ids : (row[id].state # "absent") ~> (row[id].state \in Final)

=============================================================================
