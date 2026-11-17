-------------------------- MODULE CrossBoundary --------------------------
EXTENDS Naturals, FiniteSets

\* Reduced replacement-boundary abstraction, ported from the reviewed Python
\* model. It is NOT the newer native registry/token recovery protocol.
\* ClosureObservationAvailable stands for a qualified runtime observation; the
\* implementation cannot obtain this fact by reading the model's ground truth.
CONSTANTS InitialCase, ClosureObservationAvailable, ActualChildStop,
          AllowExternalResume, Mutation

VARIABLE s
vars == <<s>>

SUP == "supervisor"
GUARD == "guardian"

NewState ==
  [service |-> TRUE, epoch |-> 0, serviceCrashed |-> FALSE,
   owner |-> "responsive", ownerFaultUsed |-> FALSE, guardian |-> TRUE,
   aliases |-> {SUP, GUARD}, authorized |-> TRUE, sent |-> FALSE,
   inflightGo |-> FALSE, pendingGo |-> FALSE, sealed |-> FALSE,
   payload |-> FALSE, stage1Starts |-> 0, resultReceived |-> FALSE,
   stopping |-> FALSE, closureObserved |-> FALSE, replacement |-> FALSE,
   newStage |-> "none", newConsumed |-> FALSE, newRunning |-> FALSE,
   badReauthorization |-> FALSE, badGenerationDelivery |-> FALSE]

InitialState ==
  CASE InitialCase = "unsent" -> NewState
    [] InitialCase = "frozen-inflight" ->
         [NewState EXCEPT !.service = FALSE, !.serviceCrashed = TRUE,
           !.owner = "frozen", !.ownerFaultUsed = TRUE,
           !.sent = TRUE, !.inflightGo = TRUE]
    [] InitialCase = "frozen-pending-restart" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.owner = "frozen", !.ownerFaultUsed = TRUE,
           !.sent = TRUE, !.pendingGo = TRUE]
    [] InitialCase = "lost-owner-payload" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.owner = "dead", !.ownerFaultUsed = TRUE, !.aliases = {GUARD},
           !.sent = TRUE, !.payload = TRUE, !.stage1Starts = 1]
    [] InitialCase = "frozen-pending-stop" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.owner = "frozen", !.ownerFaultUsed = TRUE,
           !.sent = TRUE, !.pendingGo = TRUE, !.stopping = TRUE]
    [] InitialCase = "free-lock-pending" ->
         [NewState EXCEPT !.aliases = {}, !.sent = TRUE, !.pendingGo = TRUE]
    [] InitialCase = "closed-missing-result" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.owner = "dead", !.ownerFaultUsed = TRUE, !.guardian = FALSE,
           !.aliases = {}, !.sent = TRUE, !.stage1Starts = 1]
    [] InitialCase = "closed-no-observation" ->
         [NewState EXCEPT !.owner = "dead", !.guardian = FALSE, !.aliases = {}]
    [] InitialCase = "restart-inflight" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.sent = TRUE, !.inflightGo = TRUE]
    [] InitialCase = "replacement-old-go" ->
         [NewState EXCEPT !.epoch = 1, !.serviceCrashed = TRUE,
           !.owner = "dead", !.guardian = FALSE, !.aliases = {},
           !.sent = TRUE, !.inflightGo = TRUE,
           !.closureObserved = TRUE, !.replacement = TRUE]
    [] InitialCase = "frozen-pending-service-lost" ->
         [NewState EXCEPT !.service = FALSE, !.serviceCrashed = TRUE,
           !.owner = "frozen", !.ownerFaultUsed = TRUE,
           !.sent = TRUE, !.pendingGo = TRUE]
    [] InitialCase = "frozen-pending-original-service" ->
         [NewState EXCEPT !.owner = "frozen", !.ownerFaultUsed = TRUE,
           !.sent = TRUE, !.pendingGo = TRUE]

Init == s = InitialState

PhysicallyClosed ==
  /\ ~s.payload /\ ~s.pendingGo /\ s.aliases = {}
  /\ (s.owner = "dead" \/ s.sealed)

CrashService ==
  /\ s.service /\ ~s.serviceCrashed
  /\ s' = [s EXCEPT !.service = FALSE, !.serviceCrashed = TRUE]

RestartService ==
  /\ ~s.service
  /\ s' = [s EXCEPT !.service = TRUE, !.epoch = 1]

SendOldGo ==
  /\ s.service /\ s.epoch = 0 /\ s.authorized /\ ~s.sent /\ ~s.sealed
  /\ s' = [s EXCEPT !.sent = TRUE, !.inflightGo = TRUE]

RejectSealedOldGo ==
  /\ s.owner = "responsive" /\ s.inflightGo /\ s.sealed
  /\ s' = [s EXCEPT !.inflightGo = FALSE]

ConsumeOldGo ==
  \* Local receipt may happen after service restart. No perfect failure detector.
  /\ s.owner = "responsive" /\ s.inflightGo /\ ~s.sealed
  /\ ~s.pendingGo /\ ~s.payload
  /\ s' = [s EXCEPT !.inflightGo = FALSE, !.pendingGo = TRUE]

SpawnOld ==
  /\ s.owner = "responsive" /\ s.pendingGo /\ ~s.sealed
  /\ s' = [s EXCEPT !.pendingGo = FALSE, !.payload = TRUE,
                    !.stage1Starts = @ + 1]

FreezeOwner ==
  /\ ~s.ownerFaultUsed /\ s.owner = "responsive"
  /\ s' = [s EXCEPT !.owner = "frozen", !.ownerFaultUsed = TRUE]

CrashOwner ==
  /\ ~s.ownerFaultUsed /\ s.owner = "responsive"
  /\ s' = [s EXCEPT !.owner = "dead", !.ownerFaultUsed = TRUE,
                    !.pendingGo = FALSE, !.aliases = @ \ {SUP}]

ExternalResume ==
  \* Optional environmental action, deliberately NOT fair or automatic.
  /\ AllowExternalResume /\ s.owner = "frozen"
  /\ s' = [s EXCEPT !.owner = "responsive"]

StopActualChild ==
  \* ASSUMED original-service authority and observed actual Child exit.
  \* A restarted service does not inherit this from a stored PID.
  /\ ActualChildStop /\ s.owner = "frozen" /\ s.service /\ s.epoch = 0
  /\ s' = [s EXCEPT !.owner = "dead", !.pendingGo = FALSE,
                    !.aliases = @ \ {SUP}]

RequestStop ==
  /\ ~s.stopping
  /\ s' = [s EXCEPT !.stopping = TRUE]

SealOldGate ==
  /\ s.stopping /\ s.owner = "responsive" /\ ~s.sealed
  /\ s' = [s EXCEPT !.sealed = TRUE, !.pendingGo = FALSE]

ObservePayloadStopped ==
  \* Assumed identity-safe whole payload-scope stop and observation.
  /\ s.stopping /\ s.payload /\ (s.owner = "responsive" \/ s.guardian)
  /\ s' = [s EXCEPT !.payload = FALSE]

NaturalPayloadExit ==
  \* Permitted, never assumed fair.
  /\ s.payload
  /\ s' = [s EXCEPT !.payload = FALSE]

GuardianCloseAlias ==
  /\ s.stopping /\ s.guardian /\ ~s.payload
  /\ s' = [s EXCEPT !.guardian = FALSE, !.aliases = @ \ {GUARD}]

OldOwnerCloseAndExit ==
  /\ s.owner = "responsive" /\ s.sealed /\ ~s.payload
  /\ s' = [s EXCEPT !.owner = "dead", !.aliases = @ \ {SUP}]

ObserveClosure ==
  /\ s.service /\ ~s.closureObserved /\ ClosureObservationAvailable
  /\ PhysicallyClosed
  /\ s' = [s EXCEPT !.closureObserved = TRUE]

AuthorizeReplacement ==
  /\ s.service /\ ~s.replacement
  /\ (s.closureObserved
       \/ (Mutation = "missing-receipt" /\ ~s.resultReceived)
       \/ (Mutation = "no-payload" /\ ~s.payload))
  /\ s' = [s EXCEPT !.replacement = TRUE]

AuthorizeNewStage ==
  /\ s.service /\ s.replacement /\ s.newStage = "none"
  /\ s' = [s EXCEPT !.newStage = "new"]

ReauthorizeOriginalStage ==
  /\ Mutation = "reauthorize" /\ s.service /\ s.replacement
  /\ s.newStage = "none"
  /\ s' = [s EXCEPT !.newStage = "old", !.badReauthorization = TRUE]

ReplacementReceivesOldGo ==
  /\ s.replacement /\ s.inflightGo
  /\ IF Mutation = "accept-old-generation"
        THEN s' = [s EXCEPT !.inflightGo = FALSE, !.newStage = "old",
                            !.badGenerationDelivery = TRUE]
        ELSE s' = [s EXCEPT !.inflightGo = FALSE]

SpawnReplacement ==
  /\ s.newStage # "none" /\ ~s.newConsumed /\ s.aliases = {}
  /\ s' = [s EXCEPT !.newConsumed = TRUE, !.newRunning = TRUE,
           !.stage1Starts = @ + IF s.newStage = "old" THEN 1 ELSE 0]

ReplacementExit ==
  /\ s.newRunning
  /\ s' = [s EXCEPT !.newRunning = FALSE]

Next == CrashService \/ RestartService \/ SendOldGo \/ RejectSealedOldGo
        \/ ConsumeOldGo \/ SpawnOld \/ FreezeOwner \/ CrashOwner
        \/ ExternalResume \/ StopActualChild \/ RequestStop \/ SealOldGate
        \/ ObservePayloadStopped \/ NaturalPayloadExit \/ GuardianCloseAlias
        \/ OldOwnerCloseAndExit \/ ObserveClosure \/ AuthorizeReplacement
        \/ AuthorizeNewStage \/ ReauthorizeOriginalStage
        \/ ReplacementReceivesOldGo \/ SpawnReplacement \/ ReplacementExit

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ s.service \in BOOLEAN /\ s.epoch \in 0..1
  /\ s.serviceCrashed \in BOOLEAN /\ s.ownerFaultUsed \in BOOLEAN
  /\ s.owner \in {"responsive", "frozen", "dead"}
  /\ s.guardian \in BOOLEAN /\ s.aliases \subseteq {SUP, GUARD}
  /\ s.authorized \in BOOLEAN /\ s.sent \in BOOLEAN
  /\ s.inflightGo \in BOOLEAN /\ s.pendingGo \in BOOLEAN
  /\ s.sealed \in BOOLEAN /\ s.payload \in BOOLEAN
  /\ s.stage1Starts \in 0..2 /\ s.resultReceived \in BOOLEAN
  /\ s.stopping \in BOOLEAN /\ s.closureObserved \in BOOLEAN
  /\ s.replacement \in BOOLEAN /\ s.newStage \in {"none", "old", "new"}
  /\ s.newConsumed \in BOOLEAN /\ s.newRunning \in BOOLEAN
  /\ s.badReauthorization \in BOOLEAN /\ s.badGenerationDelivery \in BOOLEAN

ClosureEvidenceCoversPayloadGateAndAliases == s.closureObserved => PhysicallyClosed
MissingReceiptOrNoPayloadIsNotClosure == s.replacement => s.closureObserved
ReplacementRequiresOldScopeStopped == s.replacement => PhysicallyClosed
ImmutableStageCannotBeReauthorized == ~s.badReauthorization
OldGenerationGoCannotAuthorizeReplacement == ~s.badGenerationDelivery
AtMostOneLaunchOfOriginalStage == s.stage1Starts <= 1
OldAndReplacementPayloadsDoNotOverlap == ~(s.payload /\ s.newRunning)

\* Reachability checks deliberately negate the target. An invariant violation
\* is a witness, not a defect in the positive protocol.
NoReplacement == ~s.replacement
NoMissingOutputNewStage == ~(s.newRunning /\ s.newStage = "new" /\ ~s.resultReceived)
NoOldStageLaunch == s.stage1Starts = 0
NoFrozenPendingAliasWitness ==
  ~(s.owner = "frozen" /\ s.pendingGo /\ ~s.payload /\ s.aliases = {SUP})
NoRejectedOldGoWitness == ~(~s.inflightGo /\ s.newStage = "none")
=============================================================================
