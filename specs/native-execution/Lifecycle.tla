--------------------------- MODULE Lifecycle ---------------------------
EXTENDS Naturals, FiniteSets, TLC

\* NOT YET PARSED OR CHECKED BY TLC: no Java runtime/TLC is installed locally.
\* Companion to check_model.py. This specification omits negative-control
\* mutations; it makes the same whole-scope cleanup/union-lock assumptions.
CONSTANTS DualOwner, MaxFaults, GuardianCanStop, ExternalContainment,
          SameDomain, ReadRead, AllowFreeze, ServiceCanStopFrozenOwner,
          ExternalCanStopFrozenOwner, FreezeService, FreezeGuardian

Jobs == {1,2}
Dom(i) == IF SameDomain THEN (IF i=1 THEN {"A","B"} ELSE {"B"})
          ELSE IF i=1 THEN {"A"} ELSE {"B"}
Conflicts(i,k) == Dom(i) \cap Dom(k) # {} /\ ~ReadRead
RequiredOwners == IF DualOwner THEN {"s","g"} ELSE {"s"}
Q == 0
A == 1
R == 2
Authorized == 3
Go == 4
Launched == 5
Closed == 6

VARIABLES service, serviceFrozen, epoch, faults, jobs
vars == <<service, serviceFrozen, epoch, faults, jobs>>
ResponsiveService == service /\ ~serviceFrozen

NewJob == [phase |-> Q, owners |-> {}, payload |-> {}, starts |-> 0,
           witnessed |-> FALSE, stopping |-> FALSE, result |-> "pending",
           generation |-> 0, leaderExit |-> FALSE, frozen |-> {}]

Init == /\ service = TRUE /\ serviceFrozen = FALSE /\ epoch = 0 /\ faults = 0
        /\ jobs = [i \in Jobs |-> NewJob]

SetJob(i,j) == /\ jobs' = [jobs EXCEPT ![i] = j]
               /\ UNCHANGED <<service,serviceFrozen,epoch,faults>>

Acquire(i) ==
  /\ ResponsiveService /\ jobs[i].phase = Q
  /\ \A k \in Jobs \ {i}:
       Conflicts(i,k) => /\ jobs[k].owners = {}
                          /\ (~ExternalContainment \/ jobs[k].payload = {})
  /\ \A k \in Jobs: (k<i /\ Conflicts(i,k)) => jobs[k].phase # Q
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = A, !.owners = {"s"}, !.generation = epoch])

QueueExpiry(i) ==
  /\ ResponsiveService /\ jobs[i].phase = Q
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = Closed, !.result = "not_started"])

Register(i) ==
  /\ jobs[i].phase = A /\ jobs[i].owners = {"s"} /\ "s" \notin jobs[i].frozen
  /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = R, !.owners = RequiredOwners])

Authorize(i) ==
  /\ ResponsiveService /\ jobs[i].phase = R /\ "s" \in jobs[i].owners
  /\ jobs[i].generation = epoch /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = Authorized])

SendGo(i) ==
  /\ ResponsiveService /\ jobs[i].phase = Authorized /\ "s" \in jobs[i].owners
  /\ jobs[i].generation = epoch /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = Go])

Spawn(i) ==
  \* Local action after GO: it may occur after service/guardian death but before
  \* that death is detected. No synchronous perfect failure detector is assumed.
  /\ jobs[i].phase = Go /\ "s" \in jobs[i].owners /\ "s" \notin jobs[i].frozen
  /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = Launched, !.payload = {"leader"},
                            !.starts = @ + 1])

ObserveSpawn(i) ==
  /\ ResponsiveService /\ jobs[i].phase = Launched /\ jobs[i].payload # {}
  /\ jobs[i].owners \ jobs[i].frozen # {} /\ ~jobs[i].witnessed
  /\ SetJob(i,[jobs[i] EXCEPT !.witnessed = TRUE])

Fork(i) ==
  /\ jobs[i].phase = Launched /\ jobs[i].payload = {"leader"}
  /\ ~jobs[i].leaderExit /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.payload = {"leader","child"}])

LeaderExit(i) ==
  /\ jobs[i].phase = Launched /\ "leader" \in jobs[i].payload
  /\ SetJob(i,[jobs[i] EXCEPT !.payload = @ \ {"leader"}, !.leaderExit = TRUE])

ChildExit(i) ==
  /\ jobs[i].phase = Launched /\ jobs[i].payload = {"child"}
  /\ SetJob(i,[jobs[i] EXCEPT !.payload = {}])

Publish(i) ==
  /\ ResponsiveService /\ jobs[i].phase = Launched /\ jobs[i].payload = {}
  /\ "s" \in jobs[i].owners /\ "s" \notin jobs[i].frozen /\ jobs[i].leaderExit
  /\ jobs[i].witnessed /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.result = "success"])

Stop(i) ==
  \* Abstract eventual deadline/loss/cancel event. Detailed numeric timers are
  \* checked separately; status polling/quiet stdout never reset deadlines.
  /\ jobs[i].phase \in A..Launched /\ ~jobs[i].stopping
  /\ SetJob(i,[jobs[i] EXCEPT !.stopping = TRUE])

Cleanup(i) ==
  /\ jobs[i].phase # Closed /\ jobs[i].stopping /\ jobs[i].payload # {}
  /\ (("s" \in jobs[i].owners /\ "s" \notin jobs[i].frozen)
       \/ ("g" \in jobs[i].owners /\ "g" \notin jobs[i].frozen /\ GuardianCanStop)
       \/ ExternalContainment)
  \* Assumes identity-safe stop and whole-scope observation eventually work.
  /\ SetJob(i,[jobs[i] EXCEPT !.payload = {}])

Release(i) ==
  LET remaining == jobs[i].owners \cap jobs[i].frozen IN
  /\ jobs[i].phase \in A..Launched /\ jobs[i].payload = {}
  /\ (jobs[i].result = "success" \/ jobs[i].stopping \/ jobs[i].owners = {})
  /\ (remaining # jobs[i].owners \/ jobs[i].owners = {})
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = IF remaining = {} THEN Closed ELSE @,
       !.owners = remaining,
       !.result = IF @ = "success" THEN @
                  ELSE IF jobs[i].phase >= Authorized THEN "unknown" ELSE "not_started"])

StopFrozenOwner(i) ==
  \E o \in {"s","g"}:
  /\ jobs[i].phase # Closed /\ jobs[i].stopping
  /\ o \in jobs[i].owners \cap jobs[i].frozen
  /\ (ExternalCanStopFrozenOwner
       \/ (o="s" /\ ServiceCanStopFrozenOwner /\ ResponsiveService /\ jobs[i].generation = epoch))
  \* Original service's actual Child or independent OS ownership is assumed.
  \* A new service generation does not acquire this authority from a saved PID.
  /\ SetJob(i,[jobs[i] EXCEPT !.owners = @ \ {o}, !.frozen = @ \ {o}])

CrashOwner(i,o) ==
  /\ faults < MaxFaults /\ jobs[i].phase # Closed /\ o \in jobs[i].owners
  /\ jobs' = [jobs EXCEPT ![i].owners = @ \ {o}, ![i].frozen = @ \ {o}]
  /\ faults' = faults + 1 /\ UNCHANGED <<service,serviceFrozen,epoch>>

CrashService ==
  /\ service /\ faults < MaxFaults /\ service' = FALSE
  /\ faults' = faults + 1 /\ UNCHANGED <<serviceFrozen,epoch,jobs>>
Restart == /\ ~service /\ service' = TRUE /\ serviceFrozen' = FALSE /\ epoch' = epoch+1
           /\ UNCHANGED <<faults,jobs>>

FreezeSupervisor(i) ==
  /\ AllowFreeze /\ faults < MaxFaults /\ jobs[i].phase # Closed
  /\ "s" \in jobs[i].owners /\ "s" \notin jobs[i].frozen
  /\ jobs' = [jobs EXCEPT ![i].frozen = @ \cup {"s"}]
  /\ faults' = faults+1 /\ UNCHANGED <<service,serviceFrozen,epoch>>
ExternalResume(i) ==
  /\ AllowFreeze /\ jobs[i].phase # Closed /\ "s" \in jobs[i].frozen
  /\ SetJob(i,[jobs[i] EXCEPT !.frozen = @ \ {"s"}])

FreezeGuardianOwner(i) ==
  /\ FreezeGuardian /\ faults < MaxFaults /\ jobs[i].phase # Closed
  /\ "g" \in jobs[i].owners /\ "g" \notin jobs[i].frozen
  /\ jobs' = [jobs EXCEPT ![i].frozen = @ \cup {"g"}]
  /\ faults' = faults+1 /\ UNCHANGED <<service,serviceFrozen,epoch>>
ResumeGuardian(i) ==
  /\ FreezeGuardian /\ jobs[i].phase # Closed /\ "g" \in jobs[i].frozen
  /\ SetJob(i,[jobs[i] EXCEPT !.frozen = @ \ {"g"}])
FreezeServiceOwner ==
  /\ FreezeService /\ service /\ ~serviceFrozen /\ faults < MaxFaults
  /\ serviceFrozen' = TRUE /\ faults' = faults+1 /\ UNCHANGED <<service,epoch,jobs>>
ResumeService == /\ FreezeService /\ service /\ serviceFrozen
                 /\ serviceFrozen' = FALSE /\ UNCHANGED <<service,epoch,faults,jobs>>

Next == CrashService \/ Restart \/ FreezeServiceOwner \/ ResumeService
        \/ \E i \in Jobs: Acquire(i) \/ QueueExpiry(i) \/ Register(i)
            \/ Authorize(i) \/ SendGo(i) \/ Spawn(i) \/ ObserveSpawn(i)
            \/ Fork(i) \/ LeaderExit(i) \/ ChildExit(i) \/ Publish(i)
            \/ Stop(i) \/ Cleanup(i) \/ Release(i) \/ StopFrozenOwner(i)
            \/ FreezeSupervisor(i) \/ ExternalResume(i)
            \/ FreezeGuardianOwner(i) \/ ResumeGuardian(i)
            \/ (\E o \in {"s","g"}: CrashOwner(i,o))

TypeOK == /\ service \in BOOLEAN /\ serviceFrozen \in BOOLEAN /\ faults \in 0..MaxFaults
          /\ epoch \in 0..MaxFaults
          /\ \A i \in Jobs: /\ jobs[i].phase \in Q..Closed
                               /\ jobs[i].owners \subseteq {"s","g"}
                               /\ jobs[i].frozen \subseteq jobs[i].owners
                               /\ jobs[i].payload \subseteq {"leader","child"}
                               /\ jobs[i].starts \in 0..1
NoConflictingPayloadAccess == Conflicts(1,2) =>
                             (jobs[1].payload = {} \/ jobs[2].payload = {})
AtMostOneLaunch == \A i \in Jobs: jobs[i].starts <= 1
ClosedMeansStopped == \A i \in Jobs: jobs[i].phase = Closed => jobs[i].payload = {}
NoFalseSuccess == \A i \in Jobs: jobs[i].result = "success" =>
                 (jobs[i].starts = 1 /\ jobs[i].payload = {} /\ jobs[i].witnessed)

Fairness == WF_vars(Restart) /\ \A i \in Jobs:
            WF_vars(QueueExpiry(i)) /\ WF_vars(Stop(i))
            /\ WF_vars(Cleanup(i)) /\ WF_vars(Release(i)) /\ WF_vars(StopFrozenOwner(i))
Spec == Init /\ [][Next]_vars /\ Fairness
EventuallyClosed == <> (\A i \in Jobs: jobs[i].phase = Closed)
=============================================================================
