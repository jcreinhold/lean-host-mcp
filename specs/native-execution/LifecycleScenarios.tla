---------------------- MODULE LifecycleScenarios -----------------------
EXTENDS Lifecycle
CONSTANTS Scenario, Mutation

RunningJob == [NewJob EXCEPT !.phase = Launched, !.owners = {"s","g"},
               !.payload = {"leader","child"}, !.starts = 1]
CaseFrozen == CASE Scenario \in {"frozen-sup", "dead-service-frozen-sup",
                                "frozen-service-and-sup"} -> {"s"}
                  [] Scenario \in {"frozen-guardian", "sup-gone-frozen-guardian"} -> {"g"}
                  [] OTHER -> {}
CaseJob == [RunningJob EXCEPT !.frozen = CaseFrozen,
            !.owners = IF Scenario = "sup-gone-frozen-guardian" THEN {"g"} ELSE @]
CaseInit ==
  /\ service = (Scenario \notin {"dead-service", "dead-service-frozen-sup"})
  /\ serviceFrozen = (Scenario \in {"frozen-service", "frozen-service-and-sup"})
  /\ epoch = 0
  /\ faults = IF Scenario \in {"dead-service-frozen-sup", "frozen-service-and-sup",
                              "sup-gone-frozen-guardian"} THEN 2 ELSE 1
  /\ jobs = [i \in Jobs |-> IF i = 1 THEN CaseJob ELSE NewJob]
ScenarioInit == IF Scenario = "normal" THEN Init ELSE CaseInit

BadFreeLock(i) ==
  /\ Mutation = "free-lock" /\ jobs[i].owners = {} /\ jobs[i].phase >= A
  /\ SetJob(i,[jobs[i] EXCEPT !.phase = Closed, !.result = "unknown"])
BadLeaderRelease(i) ==
  /\ Mutation = "leader-release" /\ jobs[i].leaderExit /\ jobs[i].payload # {}
  /\ SetJob(i,[jobs[i] EXCEPT !.owners = {}, !.phase = Closed, !.result = "success"])
BadReplay(i) ==
  /\ Mutation = "replay" /\ ResponsiveService /\ jobs[i].starts > 0
  /\ jobs[i].generation # epoch /\ jobs[i].owners = RequiredOwners
  /\ ~jobs[i].stopping /\ jobs[i].starts < 2
  /\ SetJob(i,[jobs[i] EXCEPT !.generation = epoch, !.phase = Go])
ScenarioNext == Next \/ \E i \in Jobs: BadFreeLock(i) \/ BadLeaderRelease(i) \/ BadReplay(i)
ScenarioSpec == ScenarioInit /\ [][ScenarioNext]_vars /\ Fairness
FirstScopeClosed == <> (jobs[1].phase = Closed)
NoConcurrentPayload == ~(jobs[1].payload # {} /\ jobs[2].payload # {})
NoFrozenSupervisorAliasAfterPayload ==
  ~(jobs[1].payload = {} /\ jobs[1].owners = {"s"}
    /\ jobs[1].frozen = {"s"} /\ jobs[1].phase # Closed)
=============================================================================
