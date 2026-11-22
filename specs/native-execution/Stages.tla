----------------------------- MODULE Stages -----------------------------
EXTENDS WorkerCustody, FiniteSets
CONSTANT Profile, Fault, Mutation
VARIABLE h
hv == <<h, vars>>
Commands == {1, 2}
Faults == Fault = "explore"
RecoveryRace == Fault = "recovery"
HostCapacity == Fault = "host-permit"
Contenders == {"first", "second"}
Original == <<1,1,1>>   \* job, logical stage, explicit revision
NativeId == <<"store",1,"nonce">>   \* store identity, submission window, nonce
Scope == <<"worker",2,"owner">>
PreviousScope == <<"worker",1,"owner">>
RequestKey(i) == IF Profile="warm" /\ i=2
                 THEN <<<<2,1,1>>, <<"store",1,"next-nonce">>>>
                 ELSE <<Original, NativeId>>
KeyAt(i,scope) == <<RequestKey(i), i, scope>>
CommandKey(i) == KeyAt(i,Scope)
Expected(i) == LET request == IF Profile="warm" /\ i=2 THEN RequestKey(2)
                             ELSE <<h.binding[1],h.binding[2]>>
               IN <<request,i,h.bound>>
SameOwner(a,b) == a[1]=b[1] /\ a[3]=b[3]
SameCommandWithoutGeneration(a,b) == a[1]=b[1] /\ a[2]=b[2] /\ SameOwner(a[3],b[3])
\* The old producer completed a different immutable request on the same worker
\* and owner names. Only its delayed evidence is in scope, not its physical lifetime.
OldReply == << << << <<0,1,1>>, <<"store",1,"old-nonce">> >>, 2, PreviousScope >>, TRUE >>
HostInit == Init /\ h = [
  bound |-> <<>>, sentKey |-> [i \in Commands |-> <<>>],
  emitted |-> {}, acceptedReply |-> [i \in Commands |-> <<>>],
  closure |-> <<>>, binding |-> <<>>, retry |-> "none", buffered |-> "none", submitted |-> FALSE,
  accepted |-> FALSE, rolledBackPartial |-> FALSE, domains |-> 0, permit |-> "unused",
  online |-> TRUE, crashed |-> FALSE, refused |-> FALSE, cancelled |-> FALSE, fenced |-> FALSE,
  wrongFrame |-> FALSE, readAdmitted |-> FALSE, warmSaved |-> FALSE, cmd |-> [i \in Commands |-> "idle"], starts |-> [i \in Commands |-> 0],
  report |-> "none", primary |-> "none", copied |-> "none", ack |-> FALSE, cleanupError |-> FALSE,
  unrelated |-> "queued", successor |-> "queued",
  claims |-> [c \in Contenders |-> <<>>],
  goDeliveries |-> 0, pendingTargets |-> 0, targetStarts |-> 0, goReplayedAfterStart |-> FALSE,
  startsAtFence |-> 0, racedStart |-> FALSE, fencedRunnable |-> FALSE,
  hostPermitTaken |-> FALSE, hostPermitReturns |-> 0, actorSealed |-> FALSE,
  hostClosure |-> <<>>, staleSeen |-> FALSE, duplicateSeen |-> FALSE,
  stopped |-> [committed |-> FALSE, frozen |-> FALSE, pauses |-> 0, read |-> <<>>,
    offer |-> <<>>, inserter |-> "none", coalesced |-> FALSE, request |-> <<>>,
    decision |-> "none", sampledStopped |-> FALSE, responsiveRefusal |-> FALSE,
    sent |-> {}, returned |-> {}, frozenSenders |-> {}, resumedAfterSample |-> FALSE,
    evidenceUnknown |-> FALSE, permitAtPause |-> "unused", startsAtPause |-> 0],
  deadline |-> [tick |-> 0, arm |-> "idle", armedKey |-> <<>>, finishedKey |-> <<>>,
    sentAt |-> <<>>, frozen |-> FALSE, paused |-> FALSE, expired |-> FALSE],
  writer |-> [phase |-> "new", handoff |-> <<>>, admittedHandoff |-> <<>>, valid |-> TRUE,
    checked |-> <<>>, starts |-> 0, interrupted |-> FALSE,
    stage |-> "none", stageDigest |-> <<>>, stageAck |-> <<>>,
    helperWrite |-> FALSE, callerWrite |-> FALSE,
    rollout |-> [negotiated |-> FALSE, agreed |-> {"standalone"}, drained |-> FALSE,
      selected |-> "older", rollbackDrained |-> FALSE,
      generation |-> 0, enabled |-> FALSE, captured |-> {}, publication |-> <<>>,
      revocation |-> "none"],
    result |-> "none", resultAtLoss |-> "none", outcome |-> "none",
    attestation |-> <<>>, operatorAck |-> <<>>,
    scratch |-> FALSE, cleanup |-> "none", intentBound |-> FALSE, evidence |-> <<>>,
    oldLocks |-> FALSE, held |-> FALSE, unheldCleanup |-> FALSE,
    cut |-> "none", settlements |-> 0],
  scopedLocks |-> [op \in {"legacy-reader", "private-writer", "same-output-writer", "broad-clean"}
                  |-> "idle"]]
HostOnly(change) == change /\ UNCHANGED vars
SdkOnly(action) == action /\ UNCHANGED h
Submit == HostOnly(~h.submitted /\ h.online /\ h'=[h EXCEPT !.submitted=TRUE, !.binding= <<Original,NativeId,"intent-a">>])
Accept == HostOnly(h.submitted /\ ~h.accepted /\ h.online /\ h'=[h EXCEPT !.accepted=TRUE])
Retry(intent) == HostOnly(Fault="retry" /\ h.online /\ h.submitted /\ h.retry="none"
  /\ h'=[h EXCEPT !.retry=IF intent=h.binding[3] THEN "original" ELSE "refused",
     !.binding=IF intent/=h.binding[3] /\ Mutation="rebind" THEN <<Original,NativeId,intent>> ELSE @])
\* GET and subsequent equal retries read the saved binding and disposition (stuttering).
AcquireDomain == HostOnly(h.accepted /\ h.online /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced
  /\ phase="idle" /\ h.domains<2 /\ h'=[h EXCEPT !.domains=@+1])
ReserveScope == h.online /\ h.domains=2 /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced
  /\ (~HostCapacity \/ (h.hostPermitTaken /\ ~h.actorSealed))
  /\ Reserve /\ h'=[h EXCEPT !.bound=Scope]
ConsumePermit == HostOnly(h.online /\ ~h.crashed /\ phase="reserved" /\ h.permit="unused"
  /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced /\ h'=[h EXCEPT !.permit="authorized"])
SendLaunch == HostOnly(h.online /\ ~h.crashed /\ h.permit="authorized" /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced
  /\ h'=[h EXCEPT !.permit="sent"])
ReceiveLaunch == HostOnly(~RecoveryRace /\ h.permit="sent" /\ ~h.fenced /\ phase="reserved"
  /\ h'=[h EXCEPT !.permit="consumed"])
\* One durable binding and recovery owner. A winning claim seals an unused grant.
\* A grant already issued still belongs to the old launcher until the receiver fences it.
RecoveryBinding == <<h.binding, h.bound>>
RecoveryProposals == {RecoveryBinding, <<<<Original,NativeId,"intent-b">>, Scope>>,
  <<<<Original,NativeId,"intent-a">>, PreviousScope>>}
RecoveryOwners == {c \in Contenders: h.claims[c] /= <<>> /\ h.claims[c][2]="won"}
BothExactContenders == \A c \in Contenders:
  h.claims[c] /= <<>> /\ h.claims[c][1]=RecoveryBinding
ClaimRecovery(c, proposal) ==
  LET exact == proposal=RecoveryBinding
      won == (exact \/ Mutation="accept-wrong-recovery")
        /\ (RecoveryOwners={} \/ Mutation="two-recovery-owners")
  IN HostOnly(RecoveryRace /\ phase \in {"reserved","running"} /\ supervisorAlive
    /\ ~h.fenced /\ h.claims[c]= <<>>
    /\ h'=[h EXCEPT !.claims[c]= <<proposal, IF won THEN "won" ELSE IF exact THEN "busy" ELSE "refused">>,
      !.permit=IF won /\ @="unused" THEN "sealed" ELSE @])

\* The same bound GO can arrive twice. Only its first redemption queues a target.
\* SDK Spawn starts the custody scope. An actual target start inside that scope is
\* separate, so the SDK's one-time transition cannot mask a repeated target.
OriginalGo == <<<<Original,NativeId,"intent-a">>,Scope>>
ReceiveRecoveryGo(go) ==
  LET boundGo == go=RecoveryBinding
      first == h.goDeliveries=0 /\ h.permit="sent"
      queue == boundGo /\ ~h.fenced /\ (first \/ Mutation="repeat-go")
  IN HostOnly(RecoveryRace /\ h.goDeliveries<2 /\ (h.permit="sent" \/ h.goDeliveries>0)
    /\ h'=[h EXCEPT !.goDeliveries=@+1,
      !.permit=IF queue /\ first THEN "consumed" ELSE @,
      !.goReplayedAfterStart=h.goDeliveries=1 /\ h.targetStarts=1,
      !.pendingTargets=IF queue THEN @+1 ELSE @])
ReadyRecoveryTarget == RecoveryRace /\ phase="running" /\ rootPresent /\ supervisorAlive
  /\ startupAcked /\ h.readAdmitted /\ ~h.fenced /\ h.pendingTargets>0
StartRecoveryTarget == HostOnly(ReadyRecoveryTarget
  /\ h'=[h EXCEPT !.pendingTargets=@-1, !.targetStarts=@+1,
    !.racedStart=@ \/ BothExactContenders])
RefuseSecondDomain == HostOnly(Faults /\ phase="idle" /\ h.domains=1 /\ ~h.refused /\ ~h.fenced
  /\ h'=[h EXCEPT !.refused=TRUE])
NativeLaunchFails == Faults /\ h.permit="consumed" /\ ~h.fenced /\ SpawnFails
  /\ h'=[h EXCEPT !.permit="spent", !.report="not-started"]
NativeLaunch == h.permit="consumed" /\ ~h.fenced /\ Spawn /\ h'=[h EXCEPT !.permit="spent"]
\* The surviving receiver serializes this fence against GO redemption and target start.
\* No-spawn rollback is legal only after it seals the reservation before physical spawn.
Fence == HostOnly((h.cancelled \/ h.refused \/ h.crashed \/ RecoveryOwners/={})
  /\ ~h.fenced /\ supervisorAlive
  /\ h'=[h EXCEPT !.fenced=TRUE, !.startsAtFence=h.targetStarts,
    !.fencedRunnable=ReadyRecoveryTarget /\ BothExactContenders])
Rollback == h.fenced /\ phase \in {"idle","reserved"} /\ h.domains>0
  /\ (IF phase="reserved" THEN SpawnFails ELSE UNCHANGED vars)
  /\ h'=[h EXCEPT !.rolledBackPartial=(h.domains=1), !.domains=0]
ReadyFor(i) == phase="running" /\ supervisorAlive /\ ~h.fenced /\ ~h.cancelled
  /\ ~RecoveryRace /\ ~h.actorSealed
  /\ h.online /\ ~h.crashed /\ ((startupAcked /\ h.readAdmitted) \/ Mutation="early-command")
  /\ (i=1 \/ (h.cmd[1]="valid" /\ (Profile="companion" \/ h.warmSaved)))
Sent(i) == [h EXCEPT !.cmd[i]="sent", !.sentKey[i]=Expected(i)]
Send(i) == HostOnly(ReadyFor(i) /\ h.cmd[i]="idle" /\ h'=Sent(i))
Deliver(i,frame) == HostOnly(rootPresent /\ phase="running" /\ supervisorAlive /\ ~h.fenced
  /\ h.cmd[i]="sent" /\ (frame=CommandKey(i) \/ Mutation="current-endpoint"
                       \/ (Mutation="ignore-generation" /\ SameCommandWithoutGeneration(frame,CommandKey(i))))
  /\ h'=[h EXCEPT !.cmd[i]="running", !.starts[i]=@+1, !.wrongFrame=(frame/=h.sentKey[i] \/ frame/=CommandKey(i))])
Execute(i) == Deliver(i,h.sentKey[i])
LateDelivery == Fault="late" /\ Deliver(2,CommandKey(1))
EmitReply(i,valid) == LET reply == <<CommandKey(i),valid>> IN
  HostOnly(rootPresent /\ phase="running" /\ h.cmd[i]="running" /\ (valid \/ Faults)
    /\ ~(\E r \in h.emitted: r[1]=CommandKey(i))
    /\ h'=[h EXCEPT !.emitted=@ \cup {reply}])
ReceiveReply(i,r) == HostOnly(h.online /\ h.cmd[i]="running" /\ h.primary="none"
  /\ (r[1]=h.sentKey[i] \/ (Mutation="accept-stale-reply" /\ r[1][2]=i))
  /\ h'=[h EXCEPT !.acceptedReply[i]=r[1], !.cmd[i]=IF r[2] THEN "valid" ELSE "invalid",
     !.buffered=IF ~r[2] THEN IF i=1 THEN "invalid-first" ELSE "invalid-second"
               ELSE IF i=2 THEN "success" ELSE @])
AuthenticReplies == h.emitted \cup (IF Fault="identity" THEN {OldReply} ELSE {})
Publish == HostOnly(h.online /\ h.buffered/="none" /\ h.primary="none"
  /\ h'=[h EXCEPT !.primary=h.buffered, !.buffered="none"])
Cancel == HostOnly((Faults \/ (HostCapacity /\ phase="reserved" /\ h.permit="unused"))
  /\ h.accepted /\ h.online /\ ~h.cancelled /\ h.primary="none"
  /\ h'=[h EXCEPT !.cancelled=TRUE])
CrashCaller == HostOnly(Faults /\ h.submitted /\ h.online /\ ~h.crashed
  /\ h'=[h EXCEPT !.online=FALSE, !.crashed=TRUE, !.buffered="none"])
Recover == HostOnly(~h.online /\ h'=[h EXCEPT !.online=TRUE])
\* No new execution revision on recovery. Draining the old channel or certified
\* generation retirement settles its ability to execute. Missing reply remains unknown.
SetDisposition == HostOnly(h.accepted /\ h.online /\ h.primary="none" /\ h.report="none"
  /\ h.buffered="none" /\ (h.crashed \/ h.fenced \/ phase \in {"stopping","closed"})
  /\ (~RecoveryRace \/ h.permit/="sealed" \/ phase="closed")
  /\ h'=[h EXCEPT !.report=IF h.permit="unused" \/ (RecoveryRace /\ h.permit="sealed")
                          THEN "not-started" ELSE "unknown"])
SaveCopy == HostOnly(h.online /\ h.primary/="none" /\ h.copied="none"
  /\ h'=[h EXCEPT !.copied=h.primary])
AckCopy == HostOnly(h.online /\ (h.copied/="none" \/ (Mutation="early-ack" /\ h.primary/="none")) /\ ~h.ack /\ h'=[h EXCEPT !.ack=TRUE])
SaveWarm == HostOnly(Profile="warm" /\ h.online /\ h.cmd[1]="valid" /\ ~h.warmSaved
  /\ h'=[h EXCEPT !.warmSaved=TRUE])
StopScope == supervisorAlive /\ (h.fenced \/ h.primary/="none") /\ SdkOnly(RequestStop)
CleanupError == HostOnly(Faults /\ phase="stopping" /\ ~h.cleanupError
  /\ h'=[h EXCEPT !.cleanupError=TRUE,
     !.primary=IF Mutation="overwrite-primary" /\ @/="none" THEN "cleanup-error" ELSE @])
ClosureReceipts == (IF phase="closed" THEN {Scope} ELSE {})
                   \cup (IF Fault="identity" THEN {PreviousScope} ELSE {})
\* One host actor owns its permit across requests, independently of SDK capacity.
HostPermitHeld == h.hostPermitTaken /\ h.hostPermitReturns=0
TakeHostPermit == HostOnly(HostCapacity /\ h.domains=2 /\ ~h.hostPermitTaken
  /\ h'=[h EXCEPT !.hostPermitTaken=TRUE])
SealHostActor == HostOnly(HostCapacity /\ HostPermitHeld /\ ~h.actorSealed
  /\ (h.primary/="none" \/ (h.fenced /\ h.permit="unused"))
  /\ h'=[h EXCEPT !.actorSealed=TRUE])
ConfirmedHostClosure == h.bound=Scope /\ Scope \in ClosureReceipts /\ ~capacityReserved
HostReturnEligible == HostCapacity /\ HostPermitHeld /\ h.actorSealed /\ ConfirmedHostClosure
HandleHostClosure(receipt) ==
  LET confirmed == receipt=Scope /\ ConfirmedHostClosure
      stale == receipt=PreviousScope /\ HostPermitHeld /\ h.actorSealed /\ ~h.staleSeen
      duplicate == confirmed /\ h.hostClosure=Scope /\ ~h.duplicateSeen
      returns == (confirmed /\ h.actorSealed /\ (HostPermitHeld \/
                    (Mutation="return-host-twice" /\ h.hostPermitReturns=1)))
                 \/ (stale /\ Mutation="return-stale")
  IN HostOnly(HostCapacity /\ h.hostPermitTaken /\ (returns \/ stale \/ duplicate)
    /\ h'=[h EXCEPT !.hostPermitReturns=IF returns THEN @+1 ELSE @,
      !.hostClosure=IF returns THEN receipt ELSE @, !.staleSeen=@ \/ stale,
      !.duplicateSeen=@ \/ duplicate])
HandleHostClosures == \E receipt \in {Scope, PreviousScope}: HandleHostClosure(receipt)
ReturnOnWarmRequest == HostOnly(HostCapacity /\ Mutation="return-on-request" /\ HostPermitHeld /\ h.warmSaved
  /\ h'=[h EXCEPT !.hostPermitReturns=@+1])
ReleaseDomains == \E receipt \in ClosureReceipts:
  HostOnly(h.online /\ h.bound/= <<>> /\ h.domains>0 /\ (receipt=h.bound \/ (Mutation="accept-stale-closure" /\ SameOwner(receipt,h.bound)))
    /\ h'=[h EXCEPT !.closure=receipt, !.domains=0])
UnusedReleased == h.fenced /\ phase="idle" /\ h.domains=0
AdmitSuccessor == h.online /\ h.domains=0 /\ h.successor="queued"
  /\ (IF UnusedReleased THEN UNCHANGED vars ELSE WriterAcquire)
  /\ h'=[h EXCEPT !.successor="running"]
FinishSuccessor == HostOnly(h.successor="running" /\ h'=[h EXCEPT !.successor="done"])
\* Independent ready worker, disjoint domain, one additional available slot.
Other == HostOnly(h.unrelated/="done" /\ h'=[h EXCEPT
  !.unrelated=IF @="queued" THEN "running" ELSE "done"])
LoadReads == AcquireReads /\ h'=[h EXCEPT !.readAdmitted=TRUE]
Boot == SdkOnly(GuardianReady \/ AuthenticateIdentities \/ PublishBootstrapIdentity \/ AdoptLease \/ PublishRunning \/ AcknowledgeStartup)
Maintenance == SdkOnly(DropWorkerAlias \/ GuardianDetectsOwnerLoss \/ ObserveGuardianAbsent
  \/ DiscoverGuardianLoss \/ RecoverClosedScope \/ CleanupGroup \/ PublishBootstrapClosure \/ GuardianExitsAfterGroup \/ ObserveGroupAbsent \/ Retire)
Loss == Faults /\ SdkOnly(SupervisorCrash \/ GuardianLoss \/ RootExits \/ GroupExits)
Main == Submit \/ Accept \/ AcquireDomain \/ ReserveScope \/ ConsumePermit \/ SendLaunch
  \/ ReceiveLaunch \/ NativeLaunch \/ SaveWarm \/ LoadReads \/ Boot \/ StopScope \/ Maintenance
  \/ Publish \/ SetDisposition \/ SaveCopy \/ AckCopy \/ ReleaseDomains \/ AdmitSuccessor \/ FinishSuccessor
  \/ Fence \/ Rollback \/ Recover
  \/ TakeHostPermit \/ SealHostActor \/ HandleHostClosures \/ ReturnOnWarmRequest
  \/ (\E i \in Commands: Send(i) \/ Execute(i) \/ EmitReply(i,TRUE) \/ (\E r \in AuthenticReplies: ReceiveReply(i,r)))
HostNext == (Fault="identity" /\ Deliver(2,KeyAt(2,PreviousScope))) \/ LateDelivery
  \/ RefuseSecondDomain \/ NativeLaunchFails \/ (\E intent \in {"intent-a","intent-b"}: Retry(intent))
  \/ Main \/ Other \/ Loss \/ Cancel \/ CrashCaller \/ CleanupError
  \/ (RecoveryRace /\ (\E c \in Contenders, proposal \in RecoveryProposals: ClaimRecovery(c,proposal)))
  \/ ReceiveRecoveryGo(OriginalGo) \/ StartRecoveryTarget
  \/ (\E i \in Commands: EmitReply(i,FALSE))
HostSpec == HostInit /\ [][HostNext]_hv /\ WF_hv(Main) /\ WF_hv(Other)
\* Main has finitely many non-stuttering transitions. Faults are bounded and never fair.
SafeCommands == ~h.wrongFrame /\ (\A i \in Commands: h.starts[i]<=1 /\ (h.starts[i]>0 => (startupAcked /\ h.readAdmitted)))
  /\ (h.starts[2]>0 => h.cmd[1]="valid")
  /\ ((Profile="warm" /\ h.starts[2]>0) => h.warmSaved)
  /\ (h.primary="success" => h.cmd[2]="valid")
ReplyIdentity == \A i \in Commands: h.acceptedReply[i] /= <<>> =>
  /\ h.acceptedReply[i]=Expected(i) /\ h.acceptedReply[i]=h.sentKey[i]
  /\ <<h.acceptedReply[i],h.cmd[i]="valid">> \in h.emitted
ClosureIdentity == h.closure /= <<>> => h.closure=h.bound /\ phase="closed" /\ ~capacityReserved
BoundToSdk == h.bound /= <<>> => h.bound=Scope /\ h.binding[1]=Original /\ h.binding[2]=NativeId
CompanionCompleted == <> (h.primary="success" /\ h.ack /\ h.closure=Scope
  /\ h.acceptedReply[1]=Expected(1) /\ h.acceptedReply[2]=Expected(2))
ImmutableBinding == h.submitted => h.binding= <<Original,NativeId,"intent-a">>
SafeStore == (h.ack => h.copied/="none") /\ (h.copied/="none" => h.copied=h.primary)
  /\ h.primary/="cleanup-error"
  /\ (h.report="not-started" => h.starts[1]=0 /\ h.starts[2]=0)
SafeSuccessor == h.successor/="queued" => (phase="closed" \/ UnusedReleased) /\ ~capacityReserved /\ h.domains=0
Useful == <> (h.primary="success" /\ h.ack)
SuccessorProgress == (phase="closed" \/ UnusedReleased) ~> h.successor="done"
OtherProgress == <> (h.unrelated="done")
RecoverProgress == h.crashed ~> (h.online /\ (h.report/="none" \/ h.copied/="none"))
CleanupProgress == (phase="stopping" /\ CleanupAuthority) ~> (phase="closed" \/ ~CleanupAuthority)
NoResultPendingCleanup == ~(h.primary="success" /\ h.cleanupError /\ capacityReserved)
NoBetweenCommandsCancel == ~(h.cmd[1]="valid" /\ h.cmd[2]="idle" /\ h.cancelled)
NoLostValidatedResult == ~(h.crashed /\ h.cmd[2]="valid" /\ h.report="unknown" /\ h.primary="none")
NoPartialRollback == ~(h.refused /\ h.rolledBackPartial)
NoInvalidSecondWithCleanupError == ~(h.primary="invalid-second" /\ h.cleanupError)
NoDisjointWhileUnknown == ~(phase="stopping" /\ groupPresent /\ ~CleanupAuthority /\ h.unrelated="done")
UniqueRecoveryOwner == Cardinality(RecoveryOwners)<=1
ExactRecoveryBinding == \A c \in Contenders:
  h.claims[c]/= <<>> => ((h.claims[c][2]="refused") <=> (h.claims[c][1]/=RecoveryBinding))
AtMostOneTarget == h.targetStarts<=1
FencedTargetCount == h.fenced => h.targetStarts=h.startsAtFence
NoFalseNoStart == h.report="not-started" => h.targetStarts=0
NoFenceFirst == ~(h.fencedRunnable /\ h.fenced /\ h.targetStarts=0)
NoTargetFirst == ~(h.racedStart /\ h.fenced /\ h.startsAtFence=1)
NoRejectedRecovery == ~((\A c \in Contenders: h.claims[c]/= <<>> /\ h.claims[c][2]="refused")
  /\ h.claims["first"][1]/=h.claims["second"][1])
NoSafeDuplicateGo == ~(h.goReplayedAfterStart /\ h.targetStarts=1 /\ h.pendingTargets=0 /\ ~h.fenced)
NoUnusedGoSealed == ~(h.permit="sealed" /\ BothExactContenders /\ h.report="not-started")
HostPermitSafe == h.hostPermitReturns>0 =>
  h.actorSealed /\ h.hostClosure=h.bound /\ ConfirmedHostClosure
HostPermitReturnsOnce == h.hostPermitReturns<=1
HostPermitProgress == HostReturnEligible ~> h.hostPermitReturns=1
NoChargedWarmReuse == ~(h.warmSaved /\ h.starts[2]=1 /\ HostPermitHeld /\ ~h.actorSealed)
NoStaleWithOpenScope == ~(h.staleSeen /\ phase="running" /\ capacityReserved /\ HostPermitHeld)
NoSdkReturnBeforeHost == ~(ConfirmedHostClosure /\ capacityReturns=1 /\ HostPermitHeld /\ ~h.actorSealed)
NoHostReturnWithoutAck == ~(h.duplicateSeen /\ h.hostPermitReturns=1 /\ h.copied="success" /\ ~h.ack)
NoHostReturnAfterNoStart == ~(h.hostPermitReturns=1 /\ h.fenced /\ ~runningDurable /\ capacityReturns=1)

\* This boot-qualified Running scope fixes the entire binding for one stage.
\* The original broker commits the association before the stage's GO.
Stopped == h.stopped
StartedStoppedScope == phase="running" /\ runningDurable /\ startupAcked
  /\ h.readAdmitted /\ supervisorAlive /\ h.deadline.arm="idle"
CommitStoppedBinding == HostOnly(StartedStoppedScope /\ guardianPresent /\ ~Stopped.committed
  /\ h'=[h EXCEPT !.stopped.committed=TRUE])
ConsumeStoppedPermit == HostOnly(StartedStoppedScope /\ Stopped.committed /\ ~Stopped.frozen
  /\ h.permit="unused" /\ h'=[h EXCEPT !.permit="spent"])
StoppedRecoveryPrelude == Submit \/ Accept \/ AcquireDomain \/ ReserveScope \/ SdkOnly(Spawn)
  \/ Boot \/ LoadReads \/ CommitStoppedBinding \/ ConsumeStoppedPermit
StartOriginalStoppedStage == HostOnly(StartedStoppedScope /\ ~Stopped.frozen
  /\ h.permit="spent" /\ h.targetStarts=0
  /\ h'=[h EXCEPT !.targetStarts=1])
PauseRecoveryBroker == HostOnly(StartedStoppedScope /\ Stopped.committed /\ ~Stopped.frozen /\ guardianPresent
  /\ (Stopped.pauses=0 \/ (Stopped.pauses=1 /\ Stopped.decision="refused-responsive"))
  /\ h'=[h EXCEPT !.stopped.frozen=TRUE, !.stopped.pauses=@+1, !.stopped.permitAtPause=h.permit,
    !.stopped.startsAtPause=h.targetStarts])

StoppedKeys == {Original, [Original EXCEPT ![1]=2],
  [Original EXCEPT ![2]=2], [Original EXCEPT ![3]=2]}
\* The saved capability, intent and native ID authenticate in this projection.
\* Vary each caller field independently while preserving those inputs.
ReadStoppedBinding(key) == HostOnly(Stopped.frozen /\ Stopped.committed /\ Stopped.read= <<>>
  /\ (key[1]=h.binding[1][1] \/ Mutation="drain-ignore-job")
  /\ (key[2]=h.binding[1][2] \/ Mutation="drain-ignore-stage")
  /\ (key[3]=h.binding[1][3] \/ Mutation="drain-ignore-revision")
  /\ h'=[h EXCEPT !.stopped.read= <<key,h.bound>>])

\* The envelope describes who may use this exact slot. A marker in a matched
\* generation can be empty because that generation fixes the entire binding.
SubmitStoppedDrain(sender, scope, authorized) ==
  LET offer == <<Stopped.read,scope,authorized>>
  IN HostOnly(StartedStoppedScope /\ Stopped.frozen /\ guardianPresent /\ Stopped.read/= <<>>
    /\ sender \notin Stopped.sent
    /\ (Stopped.offer= <<>> \/ Stopped.offer=offer)
    /\ h'=[h EXCEPT !.stopped.sent=@ \cup {sender},
        !.stopped.coalesced=Stopped.offer/= <<>>,
        !.stopped.inserter=IF Stopped.offer= <<>> THEN sender ELSE @,
        !.stopped.offer=offer, !.stopped.decision=IF Stopped.offer= <<>> THEN "pending" ELSE @])
FreezeStoppedSender(sender) == HostOnly(Stopped.decision="pending"
  /\ sender \in Stopped.sent \ (Stopped.returned \cup Stopped.frozenSenders)
  /\ h'=[h EXCEPT !.stopped.frozenSenders=@ \cup {sender}])
ReturnStoppedSubmission(sender) == HostOnly(sender \in Stopped.sent \ (Stopped.returned \cup Stopped.frozenSenders)
  /\ h'=[h EXCEPT !.stopped.returned=@ \cup {sender}])

\* Only the original guardian samples eligibility and publishes acceptance.
\* A positive sample can commit across a subsequent parent resume.
GuardianSampleStoppedDrain ==
  LET exactScope == Stopped.offer[2]=Stopped.read[2] \/ Mutation="drain-ignore-sdk-generation"
      authorized == Stopped.offer[3] \/ Mutation="drain-read-authority"
      decision == IF ~exactScope THEN "refused-stale"
                  ELSE IF ~authorized THEN "refused-authority"
                  ELSE IF Stopped.frozen \/ Mutation="drain-ignore-responsive" THEN "eligible"
                  ELSE "refused-responsive"
  IN HostOnly(StartedStoppedScope /\ guardianPresent /\ Stopped.decision="pending"
    /\ h'=[h EXCEPT !.stopped.decision=decision,
        !.stopped.sampledStopped=Stopped.frozen,
        !.stopped.responsiveRefusal=@ \/ decision="refused-responsive",
        !.stopped.offer=IF decision="eligible" THEN @ ELSE <<>>])
GuardianCommitStoppedDrain == HostOnly(guardianPresent /\ Stopped.decision="eligible"
  /\ (Mutation/="drain-wait-sender" \/ Stopped.frozenSenders={})
  /\ h'=[h EXCEPT !.stopped.request=Stopped.offer, !.stopped.decision="accepted"])
GuardianRequestStoppedDrain == guardianPresent
  /\ (Stopped.request/= <<>> \/ (Mutation="drain-visible-stop" /\ Stopped.decision="pending"))
  /\ SdkOnly(RequestStop)
GuardianDrainStoppedScope == Stopped.request/= <<>> /\ guardianPresent
  /\ SdkOnly(CleanupGroup \/ GuardianExitsAfterGroup)
LoseStoppedClosureEvidence == HostOnly(phase="stopping" /\ ~guardianPresent /\ ~Stopped.evidenceUnknown
  /\ (groupObservation/="absent" \/ guardianObservation/="absent")
  /\ h'=[h EXCEPT !.stopped.evidenceUnknown=TRUE])
ObserveStoppedClosure == Stopped.request/= <<>> /\ ~Stopped.evidenceUnknown
  /\ SdkOnly(ObserveGroupAbsent \/ ObserveGuardianAbsent \/ Retire)
ResumeRecoveryBroker == HostOnly(Stopped.frozen
  /\ h'=[h EXCEPT !.stopped.frozen=FALSE,
      !.stopped.resumedAfterSample=@ \/ Stopped.decision="eligible"])

ReaderStartsStoppedStage == HostOnly(Mutation="drain-read-launch" /\ StartedStoppedScope
  /\ Stopped.frozen /\ Stopped.read/= <<>> /\ h.targetStarts=Stopped.startsAtPause
  /\ h'=[h EXCEPT !.permit="authorized", !.targetStarts=@+1])
ReaderReleasesStoppedDomains == HostOnly(h.domains>0
  /\ ((Mutation="drain-read-release" /\ Stopped.frozen /\ Stopped.read/= <<>>)
      \/ (Mutation="drain-unknown-release" /\ Stopped.evidenceUnknown))
  /\ h'=[h EXCEPT !.domains=0, !.closure=h.bound])
StoppedGuardianFlow == GuardianSampleStoppedDrain \/ GuardianCommitStoppedDrain
  \/ GuardianRequestStoppedDrain \/ GuardianDrainStoppedScope \/ ObserveStoppedClosure
StoppedRecoveryNext == StoppedRecoveryPrelude \/ StartOriginalStoppedStage \/ PauseRecoveryBroker
  \/ (\E key \in StoppedKeys: ReadStoppedBinding(key))
  \/ (\E sender \in Contenders: FreezeStoppedSender(sender) \/ ReturnStoppedSubmission(sender)
      \/ (\E scope \in {Scope,PreviousScope}, authorized \in BOOLEAN: SubmitStoppedDrain(sender,scope,authorized)))
  \/ StoppedGuardianFlow \/ LoseStoppedClosureEvidence \/ SdkOnly(WriterAcquire)
  \/ ResumeRecoveryBroker \/ (~Stopped.frozen /\ ReleaseDomains)
  \/ ReaderStartsStoppedStage \/ ReaderReleasesStoppedDomains
StoppedRecoverySpec == HostInit /\ [][StoppedRecoveryNext]_hv /\ WF_hv(StoppedGuardianFlow)
StoppedDrainExact == Stopped.request/= <<>> =>
  Stopped.sampledStopped /\ Stopped.request= << <<Original,Scope>>, Scope, TRUE >>
StoppedDrainCommitted == phase \in {"stopping","closed"} => Stopped.request/= <<>>
StoppedLaunchUnchanged == Stopped.frozen =>
  h.permit=Stopped.permitAtPause /\ h.targetStarts=Stopped.startsAtPause
StoppedReleaseSafe ==
  /\ (Stopped.committed /\ phase/="closed" => h.domains=2 /\ h.closure= <<>> /\ capacityReserved)
  /\ (writerAdmitted => phase="closed" /\ ~capacityReserved /\ ~ofdLocked)
StoppedSubmissionProgress == (Stopped.decision="pending") ~> (Stopped.decision/="pending")
StoppedRecoveryProgress == (Stopped.decision \in {"eligible","accepted"})
  ~> (phase="closed" \/ Stopped.evidenceUnknown)
NoStoppedRecoveryCompletion == ~(Stopped.startsAtPause=1 /\ Stopped.resumedAfterSample
  /\ Stopped.coalesced /\ Stopped.inserter \in Stopped.frozenSenders /\ writerAdmitted /\ capacityReturns=1)
NoStoppedResponsiveRefusal == ~(Stopped.decision="refused-responsive" /\ Stopped.offer= <<>>)
NoFreshStoppedDecision == ~(Stopped.responsiveRefusal /\ Stopped.pauses=2 /\ Stopped.request/= <<>>)
NoStoppedStaleRefusal == Stopped.decision/="refused-stale"

\* Opt-in projection: command 1 only warms the single generation. Command 2
\* has one immutable deadline on a shared logical clock. No guardian loss,
\* rearming, new lease transfer, or second attempt is modeled.
HardTick == 2
WarmPrelude == Submit \/ Accept \/ AcquireDomain \/ ReserveScope \/ ConsumePermit
  \/ SendLaunch \/ ReceiveLaunch \/ NativeLaunch \/ LoadReads \/ Boot
  \/ Send(1) \/ Execute(1) \/ EmitReply(1,TRUE)
  \/ (\E r \in h.emitted: ReceiveReply(1,r)) \/ SaveWarm
DeadlineArm == HostOnly(ReadyFor(2) /\ h.warmSaved /\ guardianPresent
  /\ h.deadline.arm="idle" /\ h.deadline.tick<HardTick
  /\ h'=[h EXCEPT !.deadline.arm="armed", !.deadline.armedKey=CommandKey(2)])
DeadlineAck(key) == HostOnly(h.deadline.arm="armed" /\ phase="running"
  /\ key=h.deadline.armedKey /\ h.deadline.tick<HardTick
  /\ h'=[h EXCEPT !.deadline.arm="acked"])
\* An ambiguous ACK seals the whole generation, not just this command ID.
\* The guardian still owns the arm and must drain at the original deadline.
DeadlineLoseAck == HostOnly(phase="running" /\ h.deadline.arm="armed"
  /\ h'=[h EXCEPT !.deadline.arm="lost", !.actorSealed=TRUE])
DeadlineTick == HostOnly(h.warmSaved /\ h.deadline.tick<HardTick
  /\ h'=[h EXCEPT !.deadline.tick=@+1])
DeadlineFirstByte == HostOnly(ReadyFor(2) /\ h.cmd[2]="idle"
  /\ (h.deadline.arm="acked" \/ (Mutation="byte-without-ack" /\ h.deadline.arm="armed"))
  /\ (h.deadline.tick<HardTick \/ Mutation="byte-after-deadline")
  /\ h'=[Sent(2) EXCEPT !.deadline.sentAt= <<h.deadline.arm,h.deadline.tick>>])
\* Delivery may follow the clock edge if the first byte was already sent,
\* but not after RequestStop fences the running scope.
PauseDeadlineParent == HostOnly(phase="running" /\ h.cmd[2]="running"
  /\ h.deadline.arm="acked" /\ ~h.deadline.paused /\ supervisorAlive
  /\ h'=[h EXCEPT !.deadline.frozen=TRUE, !.deadline.paused=TRUE])
DeadlineFinish(key) == HostOnly(~h.deadline.frozen /\ phase="running"
  /\ h.deadline.arm="acked" /\ h.deadline.tick<HardTick
  /\ h.acceptedReply[2]=CommandKey(2)
  /\ (key=h.deadline.armedKey \/ Mutation="stale-finish")
  /\ h'=[h EXCEPT !.deadline.arm="finished", !.deadline.finishedKey=key])
GuardianDeadlineStop == guardianPresent /\ h.deadline.tick=HardTick
  /\ h.deadline.arm \in {"armed","acked","lost"}
  /\ (~h.deadline.frozen \/ Mutation/="parent-required-for-stop")
  /\ RequestStop /\ h'=[h EXCEPT !.deadline.expired=TRUE, !.actorSealed=TRUE]
GuardianDeadlineDrain == h.deadline.expired /\ guardianPresent /\ SdkOnly(CleanupGroup)
\* This grants no authority to report closure while the parent is paused.
\* Model eventual resume separately, then require the actual custody relation
\* for each absence observation and the single return of capacity.
ResumeDeadlineParent == HostOnly(h.deadline.frozen /\ h.deadline.expired /\ ~groupPresent
  /\ h'=[h EXCEPT !.deadline.frozen=FALSE])
ObserveDeadlineClosure == ~h.deadline.frozen /\ h.deadline.expired
  /\ (SdkOnly(GuardianExitsAfterGroup \/ ObserveGroupAbsent \/ ObserveGuardianAbsent \/ Retire)
      \/ ReleaseDomains)
DeadlineControlKeys == {CommandKey(2),KeyAt(2,PreviousScope),CommandKey(1)}
DeadlineNext == WarmPrelude \/ DeadlineArm \/ DeadlineLoseAck
  \/ (\E key \in DeadlineControlKeys: DeadlineAck(key) \/ DeadlineFinish(key))
  \/ DeadlineTick \/ DeadlineFirstByte \/ Execute(2) \/ EmitReply(2,TRUE) \/ PauseDeadlineParent
  \/ (~h.deadline.frozen /\ h.deadline.tick<HardTick /\ phase="running"
      /\ (\E r \in h.emitted: ReceiveReply(2,r)))
  \/ (h.deadline.arm="finished" /\ Publish)
  \/ GuardianDeadlineStop \/ GuardianDeadlineDrain \/ ResumeDeadlineParent \/ ObserveDeadlineClosure
DeadlineSpec == HostInit /\ [][DeadlineNext]_hv
  /\ WF_hv(DeadlineTick) /\ WF_hv(GuardianDeadlineStop) /\ WF_hv(GuardianDeadlineDrain)
  /\ WF_hv(ResumeDeadlineParent) /\ WF_hv(ObserveDeadlineClosure)
DeadlineByteOrder ==
  /\ (h.cmd[2]/="idle") = (h.deadline.sentAt /= <<>>)
  /\ (h.deadline.sentAt /= <<>> =>
      h.deadline.sentAt[1]="acked" /\ h.deadline.sentAt[2]<HardTick
      /\ h.deadline.armedKey=h.sentKey[2])
DeadlineFinishIdentity == h.deadline.arm="finished" =>
  h.deadline.finishedKey=CommandKey(2) /\ h.acceptedReply[2]=CommandKey(2)
DeadlineLostAckSealed == h.deadline.arm="lost" =>
  h.actorSealed /\ h.deadline.sentAt= <<>> /\ h.starts[2]=0 /\ h.primary="none"
DeadlineClosureEvidence ==
  /\ (h.deadline.frozen => h.closure= <<>> /\ capacityReserved /\ capacityReturns=0)
  /\ (phase="closed" => h.deadline.expired /\ ~groupPresent /\ ~guardianPresent
      /\ aliases={} /\ ~ofdLocked /\ groupObservation="absent" /\ guardianObservation="absent"
      /\ ~capacityReserved /\ capacityReturns=1)
FrozenGuardianStops == h.deadline.frozen ~> (h.deadline.expired /\ ~groupPresent)
FrozenGuardianRetires == h.deadline.paused ~> (h.closure=Scope /\ capacityReturns=1)
LostAckRetires == (h.deadline.arm="lost") ~> (h.closure=Scope /\ capacityReturns=1)
NoFrozenDrain == ~(h.deadline.frozen /\ h.deadline.expired /\ ~groupPresent)
NoLostAckRetirement == ~(h.deadline.arm="lost" /\ h.closure=Scope /\ capacityReturns=1)

\* Opt-in writer projection: an exact SDK reader closure releases the host's
\* domain holds before the old writer turn. The overlapping successor still
\* uses the SDK WriterAcquire through AdmitSuccessor. Other has its own scope.
Writer == h.writer
HelperOwnedTurn == Profile="writer-helper-staging"
CommandOnlyTurn == Profile="writer-command-only" \/ HelperOwnedTurn
WriterRolloutCase == HelperOwnedTurn /\ Fault="writer-rollout"
WriterRollout == Writer.rollout
SupportedWriterLaunchers == {"standalone","embedded"}
WriterRolloutReady == WriterRollout.negotiated /\ WriterRollout.drained
  /\ WriterRollout.agreed=SupportedWriterLaunchers
ExtendedWriterCharge == WriterRolloutCase /\ Writer.phase/="new" /\ Writer.settlements=0
EnabledWriterGeneration == IF WriterRollout.enabled THEN WriterRollout.generation ELSE 0
WriterApprovalRequests == IF WriterRolloutCase THEN WriterRollout.captured ELSE {0}
OriginalHandoff == [source |-> Original, store |-> NativeId, file |-> "store-file-a",
  manifest |-> "manifest-a", audit |-> "audit-a", notices |-> "notices-a"]
WriterInputs == {OriginalHandoff,
  [OriginalHandoff EXCEPT !.store= <<"other-store",1,"nonce">>],
  [OriginalHandoff EXCEPT !.file="restored-file"],
  [OriginalHandoff EXCEPT !.manifest="manifest-b"],
  [OriginalHandoff EXCEPT !.audit="audit-b"],
  [OriginalHandoff EXCEPT !.notices="notices-b"]}
WriterLockUnion == << <<"project", "device-a", "inode-a">>,
                      <<"dependency", "device-a", "inode-b">> >>
WriterOwner == <<"writer",2,"writer-owner">>
PreviousWriterOwner == <<"writer",1,"writer-owner">>
\* A symbolic digest binds all captured identities; no hash or durability claim.
WriterDigest(input) == <<"handoff-digest", input>>
WriterReservation == [turn |-> "turn-a", owner |-> WriterOwner, registry |-> "registry-a",
  digest |-> WriterDigest(OriginalHandoff), locks |-> WriterLockUnion,
  scratch |-> <<"scratch-a", "device-a", "inode-c">>]
ReversedWriterEvidence == [WriterReservation EXCEPT !.locks= <<WriterLockUnion[2],WriterLockUnion[1]>>]
WriterEvidence == {WriterReservation, ReversedWriterEvidence,
  [WriterReservation EXCEPT !.owner=PreviousWriterOwner],
  [WriterReservation EXCEPT !.digest=WriterDigest([OriginalHandoff EXCEPT !.audit="audit-b"])],
  [WriterReservation EXCEPT !.locks= <<WriterLockUnion[1]>>],
  [WriterReservation EXCEPT !.locks= <<WriterLockUnion[1], <<"dependency","device-a","other-inode">>>>],
  [WriterReservation EXCEPT !.scratch= <<"scratch-a","device-a","other-inode">>]}
ExactWriterEvidenceAvailable == Fault \notin {"writer-unverifiable","writer-operator-unverifiable"}
OfferedWriterEvidence == IF ExactWriterEvidenceAvailable THEN WriterEvidence
  ELSE {e \in WriterEvidence: e/=WriterReservation}
OperatorCase == ~CommandOnlyTurn /\ Fault \in {"writer-operator","writer-operator-ack-only",
  "writer-operator-stale","writer-operator-closure-only","writer-operator-unverifiable"}
WriterTurnKey == <<WriterReservation.turn,WriterReservation.owner,WriterReservation.registry>>
StaleWriterTurnKey == <<"previous-turn",WriterReservation.owner,WriterReservation.registry>>
OperatorAttestations == CASE Fault="writer-operator-ack-only" -> {}
  [] Fault="writer-operator-stale" -> {StaleWriterTurnKey}
  [] OTHER -> {WriterTurnKey,StaleWriterTurnKey}
ExactOperatorClosure == Writer.attestation=WriterTurnKey
ExpectedOperatorAck == <<WriterTurnKey,Writer.outcome>>
ExactOperatorAck == Writer.operatorAck=ExpectedOperatorAck
OperatorClosureAuthorized == OperatorCase /\ ExactOperatorClosure /\ ExactOperatorAck

WriterReader == Submit \/ Accept \/ AcquireDomain \/ ReserveScope \/ ConsumePermit
  \/ SendLaunch \/ ReceiveLaunch \/ NativeLaunch \/ LoadReads \/ Boot
  \/ (h.readAdmitted /\ startupAcked /\ SdkOnly(RequestStop))
  \/ Maintenance \/ ReleaseDomains
CaptureWriterHandoff == HostOnly(Writer.handoff= <<>>
  /\ h'=[h EXCEPT !.writer.handoff=OriginalHandoff])
\* Invalidation can retain every original identifier (edit and restore).
InvalidateWriterInput == HostOnly(Writer.handoff/= <<>> /\ Writer.checked= <<>> /\ Writer.valid
  /\ h'=[h EXCEPT !.writer.valid=FALSE])
WriterReadyToReserve == Writer.phase="new" /\ Writer.handoff=OriginalHandoff
  /\ h.closure=Scope /\ h.domains=0
\* Approval comparison and charge publication share the registry admission lock.
ReserveWriter(requested) == HostOnly(WriterReadyToReserve
  /\ (~WriterRolloutCase \/ (WriterRollout.selected="extended"
      /\ (requested=EnabledWriterGeneration
          \/ (Mutation="writer-reserve-revoked" /\ ~WriterRollout.enabled)
          \/ (Mutation="writer-reserve-superseded" /\ WriterRollout.enabled))))
  /\ h'=[h EXCEPT !.writer.phase="idle", !.writer.scratch=TRUE, !.writer.oldLocks=TRUE,
     !.writer.admittedHandoff=WriterReservation,
     !.writer.rollout.publication=IF WriterRolloutCase THEN <<requested,EnabledWriterGeneration>> ELSE @,
     !.writer.rollout.rollbackDrained=FALSE])
ValidWriterInput(input) == WriterDigest(input)=Writer.admittedHandoff.digest /\ Writer.valid

\* The admission receipt and durable Idle precede staging. Only the helper
\* takes a writable descriptor under the original ordered locks.
UnderOriginalWriterLocks == Writer.oldLocks /\ Writer.admittedHandoff.locks=WriterLockUnion
BeginHelperStaging(input) == HostOnly(HelperOwnedTurn /\ Writer.phase="idle" /\ ~Writer.interrupted
  /\ UnderOriginalWriterLocks /\ Writer.stage="none"
  /\ WriterDigest(input)=Writer.admittedHandoff.digest
  /\ h'=[h EXCEPT !.writer.stage="writing", !.writer.stageDigest=WriterDigest(input),
     !.writer.helperWrite=TRUE, !.writer.callerWrite=Mutation="writer-caller-stage-write"])
\* One abstract commit covers publishing the verified file and closing the
\* helper's write descriptor. The subsequent acknowledgment is separate.
CommitHelperStaging == HostOnly(HelperOwnedTurn /\ ~Writer.interrupted /\ UnderOriginalWriterLocks
  /\ Writer.stage="writing" /\ Writer.helperWrite /\ Writer.stageDigest=Writer.admittedHandoff.digest
  /\ h'=[h EXCEPT !.writer.stage="published", !.writer.helperWrite=FALSE])
ExactStageAck == <<WriterTurnKey,WriterReservation.digest>>
AcknowledgeHelperStaging == HostOnly(HelperOwnedTurn /\ ~Writer.interrupted /\ UnderOriginalWriterLocks
  /\ Writer.stage="published" /\ ~Writer.helperWrite /\ Writer.stageAck= <<>>
  /\ h'=[h EXCEPT !.writer.stageAck= <<WriterTurnKey,Writer.stageDigest>>])
WriterStageReady == Writer.stage="published" /\ Writer.stageAck=ExactStageAck

\* The helper profile validates the source inside the supervised child.
\* Matching staging bytes alone do not establish current source validity.
ValidateWriterInput(input) == HostOnly(Writer.phase=(IF HelperOwnedTurn THEN "running" ELSE "idle")
  /\ ~Writer.interrupted /\ Writer.checked= <<>>
  /\ (ValidWriterInput(input)
      \/ (Mutation="writer-recapture" /\ Writer.valid)
      \/ (Mutation="writer-ignore-invalidity" /\ input=Writer.handoff))
  /\ h'=[h EXCEPT !.writer.checked=input])
\* Persist the non-idle phase before GO. An already issued GO may arrive
\* after loss, but recovery never issues another GO for that turn.
ArmWriter == HostOnly(Writer.phase="idle" /\ ~Writer.interrupted /\ Writer.starts=0
  /\ (IF HelperOwnedTurn THEN WriterStageReady \/ Mutation="writer-go-before-stage"
      ELSE Writer.checked/= <<>>)
  /\ h'=[h EXCEPT !.writer.phase="starting"])
RunWriter == HostOnly(((Writer.phase="starting" /\ Writer.starts=0)
  \/ (Mutation="writer-replay" /\ Writer.interrupted /\ Writer.phase="running" /\ Writer.starts=1))
  /\ h'=[h EXCEPT !.writer.phase="running", !.writer.starts=@+1])
\* The stop action assumes authentic observation that the command and all
\* delegated writes stopped. Until all Idle records agree, recovery is fenced.
StopWriter == HostOnly(Writer.phase="running"
  /\ h'=[h EXCEPT !.writer.phase="partial-idle"])
PublishWriterIdle == HostOnly(Writer.phase="partial-idle" /\ ~Writer.interrupted
  /\ h'=[h EXCEPT !.writer.phase="idle"])
SaveWriterResult == HostOnly(Writer.phase \in {"partial-idle","idle"} /\ Writer.starts=1
  /\ ~Writer.interrupted /\ Writer.result="none"
  /\ (~HelperOwnedTurn \/ Writer.checked/= <<>>)
  /\ h'=[h EXCEPT !.writer.result="success"])
\* An unconfirmed close says nothing new about the command's result.
WriterLossOutcome == IF Writer.resultAtLoss="none" THEN "unknown" ELSE Writer.resultAtLoss
InterruptWriter == HostOnly(Writer.phase \in {"idle","starting","running","partial-idle"} /\ ~Writer.interrupted
  /\ h'=[h EXCEPT !.writer.interrupted=TRUE, !.writer.resultAtLoss=Writer.result,
     !.writer.outcome=IF Writer.result="none" THEN "unknown" ELSE Writer.result])
WriterMayWrite == Writer.phase \in {"starting","running"}
  \/ (~CommandOnlyTurn /\ Writer.phase \in {"idle","partial-idle"})
  \/ Writer.helperWrite \/ Writer.callerWrite
\* This models an observed helper death. Its descriptor closes even if
\* scratch is partial. Neither lock release nor cleanup confers closure.
LoseHelperStagingAuthority == HostOnly(HelperOwnedTurn /\ Writer.interrupted /\ Writer.helperWrite
  /\ h'=[h EXCEPT !.writer.helperWrite=FALSE])
\* Old kernel locks can close before the reservation settles. The charge,
\* not descriptor liveness, continues to fence conflicting admission.
DropOldWriterLocks == HostOnly(Writer.interrupted /\ Writer.oldLocks
  /\ h'=[h EXCEPT !.writer.oldLocks=FALSE])
InterruptedCommandOnlyIdle == CommandOnlyTurn /\ Writer.interrupted /\ Writer.phase="idle" /\ ~Writer.oldLocks
WriterIdleRecovery == InterruptedCommandOnlyIdle /\ ~Writer.helperWrite /\ ~Writer.callerWrite

\* These inputs attest to a named supported launcher set. Caller negotiation
\* alone observes only the selected helper. It says nothing about other launchers.
NegotiateWriterRollout == HostOnly(WriterRolloutCase /\ ~WriterRollout.negotiated
  /\ h'=[h EXCEPT !.writer.rollout.negotiated=TRUE])
ObserveWriterLauncherAgreement == HostOnly(WriterRolloutCase /\ WriterRollout.selected="older"
  /\ WriterRollout.agreed/=SupportedWriterLaunchers
  /\ h'=[h EXCEPT !.writer.rollout.agreed=SupportedWriterLaunchers])
ObserveWriterSwitchDrain == HostOnly(WriterRolloutCase /\ WriterRollout.selected="older" /\ ~WriterRollout.drained
  /\ h'=[h EXCEPT !.writer.rollout.drained=TRUE])
PublishWriterApproval == HostOnly(WriterRolloutCase /\ WriterRollout.selected/="rolled-back"
  /\ ~WriterRollout.enabled /\ WriterRollout.generation<2
  /\ (WriterRolloutReady
      \/ (Mutation="writer-negotiate-only" /\ WriterRollout.negotiated /\ WriterRollout.drained))
  /\ h'=[h EXCEPT !.writer.rollout.selected="extended", !.writer.rollout.generation=@+1,
     !.writer.rollout.enabled=TRUE, !.writer.rollout.revocation="none", !.writer.rollout.rollbackDrained=FALSE])
CaptureWriterApproval == HostOnly(WriterRolloutCase /\ Writer.phase="new" /\ WriterRollout.enabled
  /\ WriterRollout.generation \notin WriterRollout.captured
  /\ h'=[h EXCEPT !.writer.rollout.captured=@ \cup {WriterRollout.generation}])
\* Revocation commits disable under the same lock, including a busy result.
\* A later scan may confirm an empty ledger without changing captured tokens.
WriterRevocationResult == IF ExtendedWriterCharge THEN "busy" ELSE "clear"
RevokeWriterApproval == HostOnly(WriterRolloutCase /\ WriterRollout.selected="extended"
  /\ (WriterRollout.enabled \/ WriterRollout.revocation/=WriterRevocationResult)
  /\ h'=[h EXCEPT !.writer.rollout.revocation=WriterRevocationResult,
     !.writer.rollout.enabled=IF Mutation="writer-busy-revoke-enabled" /\ ExtendedWriterCharge THEN @ ELSE FALSE])
\* Supported-launcher quiescence and ledger settlement are separate observations.
ObserveWriterRollbackDrain == HostOnly(WriterRolloutCase /\ WriterRollout.selected="extended"
  /\ ~WriterRollout.enabled
  /\ (Writer.phase="new" \/ (Writer.interrupted /\ ~Writer.oldLocks /\ ~WriterMayWrite))
  /\ ~WriterRollout.rollbackDrained
  /\ h'=[h EXCEPT !.writer.rollout.rollbackDrained=TRUE])
SelectUnawareWriter == HostOnly(WriterRolloutCase /\ WriterRollout.selected="extended"
  /\ ~WriterRollout.enabled
  /\ (WriterRollout.rollbackDrained \/ Mutation="writer-rollback-without-drain")
  /\ ((WriterRollout.revocation="clear" /\ ~ExtendedWriterCharge)
      \/ (Mutation="writer-rollback-unsettled" /\ WriterRollout.revocation="busy" /\ ExtendedWriterCharge))
  /\ h'=[h EXCEPT !.writer.rollout.selected="rolled-back"])

\* An independently authenticated fact for this exact turn certifies that
\* every old write authority has stopped, including any pending GO. The
\* model does not manufacture it from a boot change or PID observation.
ReceiveOperatorAttestation(attestation) == HostOnly(OperatorCase /\ Writer.interrupted
  /\ ~ExactOperatorClosure /\ attestation/=Writer.attestation
  /\ h'=[h EXCEPT !.writer.attestation=attestation,
     !.writer.phase=IF attestation=WriterTurnKey THEN "operator-closed" ELSE @])
AcknowledgeOperatorRecovery == HostOnly(OperatorCase /\ Fault/="writer-operator-closure-only"
  /\ Writer.interrupted /\ Writer.operatorAck= <<>>
  /\ h'=[h EXCEPT !.writer.operatorAck=ExpectedOperatorAck])
OperatorWriterRecovery == OperatorClosureAuthorized /\ Writer.phase="operator-closed"
WriterRecoveryAuthorized == WriterIdleRecovery \/ OperatorWriterRecovery
WriterRecoveryInputs == ExactWriterEvidenceAvailable /\ (Writer.scratch \/ Writer.cleanup/="none")

\* Both routes reacquire the original ordered EX union and prove the bound
\* scratch before writing intent. After intent, absence can resume cleanup.
FindUnmarkedScratchAbsent == HostOnly(Writer.interrupted /\ ~Writer.oldLocks
  /\ Writer.scratch /\ Writer.cleanup="none"
  /\ h'=[h EXCEPT !.writer.scratch=FALSE])
ClaimWriterCleanup(evidence) == HostOnly(Writer.interrupted /\ ~Writer.oldLocks /\ ~Writer.held
  /\ (WriterRecoveryAuthorized
      \/ (Mutation="writer-recover-legacy" /\ Writer.phase="idle")
      \/ (Mutation="writer-accept-ack-only" /\ OperatorCase /\ ExactOperatorAck /\ Writer.attestation= <<>>)
      \/ (Mutation="writer-accept-stale-attestation" /\ OperatorCase /\ ExactOperatorAck
          /\ Writer.attestation=StaleWriterTurnKey)
      \/ (Mutation="writer-accept-closure-only" /\ OperatorCase /\ ExactOperatorClosure)
      \/ (Mutation="writer-unknown-is-idle" /\ Writer.phase \in {"starting","running","partial-idle"})
      \/ (Mutation="writer-caller-stage-write" /\ HelperOwnedTurn /\ InterruptedCommandOnlyIdle
          /\ ~Writer.helperWrite))
  /\ (evidence=WriterReservation
      \/ (Mutation="writer-partial-union" /\ evidence.locks= <<WriterLockUnion[1]>>)
      \/ (Mutation="writer-reversed-union" /\ evidence=ReversedWriterEvidence)
      \/ (Mutation="writer-stale-owner" /\ evidence.owner=PreviousWriterOwner)
      \/ (Mutation="writer-stale-digest" /\ evidence.digest/=WriterReservation.digest)
      \/ (Mutation="writer-wrong-scratch" /\ evidence.scratch/=WriterReservation.scratch))
  /\ (Writer.scratch \/ Writer.cleanup/="none" \/ Mutation="writer-unmarked-absence")
  /\ h'=[h EXCEPT !.writer.held=TRUE, !.writer.evidence=evidence,
     !.writer.intentBound=IF Writer.cleanup="none" THEN Writer.scratch ELSE @,
     !.writer.cleanup=IF @="none" THEN "intent" ELSE @])
RemoveWriterScratch == HostOnly(Writer.cleanup="intent" /\ Writer.scratch
  /\ (Writer.held \/ (Mutation="writer-skip-reacquire" /\ Writer.cut="intent"))
  /\ h'=[h EXCEPT !.writer.scratch=FALSE, !.writer.unheldCleanup=~Writer.held])
SyncWriterCleanup == HostOnly(Writer.held /\ Writer.cleanup="intent" /\ ~Writer.scratch
  /\ h'=[h EXCEPT !.writer.cleanup="synced"])
InterruptWriterCleanup == HostOnly(Writer.held /\ Writer.cut="none"
  /\ h'=[h EXCEPT !.writer.held=FALSE,
     !.writer.cut=IF Writer.cleanup="synced" THEN "synced" ELSE IF Writer.scratch THEN "intent" ELSE "removed"])
SettleWriter == HostOnly(Mutation/="writer-stall-settlement"
  /\ ((Writer.held /\ Writer.cleanup="synced" /\ Writer.settlements=0)
      \/ (Mutation="writer-double-settle" /\ Writer.settlements=1))
  /\ h'=[h EXCEPT !.writer.phase="settled", !.writer.held=FALSE, !.writer.settlements=@+1,
     !.writer.outcome=IF Mutation="writer-rewrite-outcome" THEN "settled" ELSE @])
WriterOverlap == (Writer.settlements=1
  \/ (Mutation="writer-overlap-early" /\ Writer.interrupted /\ ~Writer.oldLocks /\ WriterMayWrite))
  /\ AdmitSuccessor
WriterOther == Writer.interrupted /\ (Mutation/="writer-global-fence" \/ Writer.settlements=1) /\ Other
WriterFlow == WriterReader \/ CaptureWriterHandoff
  \/ (\E requested \in WriterApprovalRequests: ReserveWriter(requested))
  \/ NegotiateWriterRollout \/ ObserveWriterLauncherAgreement \/ ObserveWriterSwitchDrain
  \/ PublishWriterApproval \/ CaptureWriterApproval \/ RevokeWriterApproval \/ ObserveWriterRollbackDrain
  \/ SelectUnawareWriter
  \/ (\E input \in WriterInputs: BeginHelperStaging(input)) \/ CommitHelperStaging \/ AcknowledgeHelperStaging
  \/ (\E input \in WriterInputs: ValidateWriterInput(input))
  \/ ArmWriter \/ RunWriter \/ PublishWriterIdle \/ SaveWriterResult
  \/ (\E evidence \in OfferedWriterEvidence: ClaimWriterCleanup(evidence))
  \/ RemoveWriterScratch \/ SyncWriterCleanup \/ SettleWriter \/ WriterOverlap \/ FinishSuccessor
WriterNext == WriterFlow \/ WriterOther \/ InvalidateWriterInput \/ InterruptWriter
  \/ StopWriter \/ DropOldWriterLocks \/ LoseHelperStagingAuthority \/ FindUnmarkedScratchAbsent
  \/ InterruptWriterCleanup
  \/ (\E attestation \in OperatorAttestations: ReceiveOperatorAttestation(attestation))
  \/ AcknowledgeOperatorRecovery
WriterSpec == HostInit /\ [][WriterNext]_hv /\ WF_hv(WriterFlow) /\ WF_hv(WriterOther)

WriterHandoffSafe ==
  /\ (Writer.handoff/= <<>> => Writer.handoff=OriginalHandoff)
  /\ (Writer.phase/="new" => Writer.admittedHandoff=WriterReservation)
  /\ (Writer.checked/= <<>> => ValidWriterInput(Writer.checked))
  /\ ((HelperOwnedTurn /\ Writer.result="success") => Writer.checked=OriginalHandoff /\ Writer.valid)
WriterStageSafe ==
  /\ ~Writer.callerWrite
  /\ (Writer.helperWrite => HelperOwnedTurn /\ Writer.stage="writing")
  /\ (Writer.stage="published" => Writer.stageDigest=WriterReservation.digest /\ ~Writer.helperWrite)
  /\ (Writer.stageAck/= <<>> => WriterStageReady)
  /\ ((HelperOwnedTurn /\ Writer.starts>0) => WriterStageReady)
WriterStageSettlesClosed ==
  (HelperOwnedTurn /\ Writer.settlements>0) => ~Writer.helperWrite /\ ~Writer.callerWrite
WriterRolloutAdmissionSafe == (WriterRolloutCase /\ Writer.phase/="new") => WriterRolloutReady
WriterRolloutApprovalSafe == (WriterRolloutCase /\ Writer.phase/="new") =>
  WriterRollout.publication \in {<<1,1>>,<<2,2>>}
WriterRolloutRevocationSafe == (WriterRolloutCase /\ WriterRollout.revocation/="none") => ~WriterRollout.enabled
WriterRolloutRollbackSafe == (WriterRolloutCase /\ WriterRollout.selected="rolled-back") =>
  WriterRollout.rollbackDrained /\ ~WriterRollout.enabled
  /\ WriterRollout.revocation="clear" /\ ~ExtendedWriterCharge
WriterNoReplay == Writer.starts<=1
WriterOperatorSafe ==
  /\ (Writer.operatorAck/= <<>> => OperatorCase /\ Writer.interrupted /\ ExactOperatorAck)
  /\ (Writer.phase="operator-closed" => OperatorCase /\ Writer.interrupted /\ ExactOperatorClosure)
WriterCleanupSafe == Writer.cleanup/="none" =>
  Writer.interrupted
  /\ (IF CommandOnlyTurn THEN Writer.phase \in {"idle","settled"}
      ELSE OperatorClosureAuthorized /\ Writer.phase \in {"operator-closed","settled"})
  /\ ~Writer.oldLocks /\ ~WriterMayWrite /\ Writer.intentBound /\ Writer.evidence=WriterReservation
  /\ ~Writer.unheldCleanup /\ (Writer.cleanup="synced" => ~Writer.scratch)
WriterSettlesOnce == Writer.settlements<=1 /\ (Writer.settlements=1 => Writer.cleanup="synced")
WriterOutcomeStable == Writer.interrupted =>
  Writer.outcome=WriterLossOutcome /\ Writer.result=Writer.resultAtLoss
WriterOverlapSafe == h.successor/="queued" =>
  Writer.settlements=1 /\ Writer.phase="settled" /\ ~WriterMayWrite /\ writerAdmitted
WriterUnverifiableFenced == ~ExactWriterEvidenceAvailable =>
  Writer.settlements=0 /\ h.successor="queued"
WriterSafety == WriterHandoffSafe /\ WriterStageSafe /\ WriterStageSettlesClosed /\ WriterNoReplay /\ WriterCleanupSafe
  /\ WriterSettlesOnce /\ WriterOutcomeStable /\ WriterOverlapSafe /\ WriterUnverifiableFenced /\ WriterOperatorSafe
  /\ WriterRolloutAdmissionSafe /\ WriterRolloutApprovalSafe /\ WriterRolloutRevocationSafe /\ WriterRolloutRollbackSafe
WriterRecoveryProgress ==
  [] ((WriterRecoveryAuthorized /\ ~Writer.oldLocks /\ WriterRecoveryInputs)
      => (([] WriterRecoveryInputs) => <> (Writer.settlements=1)))
WriterDisjointProgress == Writer.interrupted ~> h.unrelated="done"
WriterSuccessorProgress == Writer.settlements=1 ~> h.successor="done"
NoWriterUnlockedDisjoint == ~(Writer.interrupted /\ ~Writer.oldLocks /\ WriterMayWrite /\ h.unrelated="done")
NoLegacyIdleDisjoint == ~(~CommandOnlyTurn /\ Writer.interrupted /\ ~Writer.oldLocks
  /\ Writer.phase="idle" /\ Writer.starts=1 /\ h.unrelated="done")
NoWriterUnverifiableIdle == ~(~ExactWriterEvidenceAvailable /\ WriterIdleRecovery
  /\ Writer.starts=1 /\ Writer.scratch /\ h.unrelated="done")
NoWriterPartialIdleFence == ~(Writer.interrupted /\ Writer.phase="partial-idle" /\ h.unrelated="done")
NoWriterInvalidHandoffFence == ~(~Writer.valid /\ Writer.interrupted /\ Writer.starts=0 /\ h.unrelated="done")
NoWriterUnknownRetirement == ~(Writer.outcome="unknown" /\ Writer.starts=1 /\ h.successor="done")
NoWriterSavedRetirement == ~(Writer.outcome="success" /\ h.successor="done")
NoWriterIntentRecovery == ~(Writer.cut="intent" /\ Writer.settlements=1)
NoWriterRemovalRecovery == ~(Writer.cut="removed" /\ Writer.settlements=1)
NoWriterSyncRecovery == ~(Writer.cut="synced" /\ Writer.settlements=1)
NoPartialHelperStageRetirement ==
  ~(Writer.stage="writing" /\ ~Writer.helperWrite /\ Writer.starts=0
    /\ Writer.outcome="unknown" /\ Writer.settlements=1)
NoPublishedHelperStageRetirement ==
  ~(Writer.stage="published" /\ Writer.stageAck= <<>> /\ Writer.starts=0
    /\ Writer.outcome="unknown" /\ Writer.settlements=1)
NoHelperStageValidatedGo ==
  ~(WriterStageReady /\ Writer.starts=1 /\ Writer.checked=OriginalHandoff /\ Writer.result="success")
NoHelperStageInvalidSourceRetirement ==
  ~(WriterStageReady /\ ~Writer.valid /\ Writer.starts=1 /\ Writer.checked= <<>>
    /\ Writer.result="none" /\ Writer.settlements=1)
NoMixedWriterNegotiation ==
  ~(WriterRollout.negotiated /\ WriterRollout.drained /\ WriterRollout.agreed/=SupportedWriterLaunchers
    /\ WriterRollout.selected="older" /\ WriterReadyToReserve)
NoDrainedWriterRollback ==
  ~(WriterRollout.selected="rolled-back" /\ WriterRollout.rollbackDrained /\ Writer.settlements=1
    /\ Writer.outcome="unknown")
NoBusyWriterRevocation ==
  ~(ExtendedWriterCharge /\ WriterRollout.revocation="busy" /\ ~WriterRollout.enabled
    /\ WriterRollout.publication= <<1,1>>)
NoReapprovedWriterPublication ==
  ~(WriterRollout.publication= <<2,2>> /\ 1 \in WriterRollout.captured)
OperatorFenceExercised == ~Writer.oldLocks /\ h.unrelated="done"
NoOperatorAckOnlyDisjoint == ~(ExactOperatorAck /\ Writer.attestation= <<>> /\ OperatorFenceExercised)
NoOperatorStaleDisjoint == ~(ExactOperatorAck /\ Writer.attestation=StaleWriterTurnKey /\ OperatorFenceExercised)
NoOperatorClosureOnlyDisjoint == ~(ExactOperatorClosure /\ Writer.operatorAck= <<>> /\ OperatorFenceExercised)
NoOperatorUnverifiableDisjoint == ~(OperatorWriterRecovery /\ ~ExactWriterEvidenceAvailable /\ OperatorFenceExercised)
NoOperatorRetirementAfterRemoval ==
  ~(OperatorClosureAuthorized /\ Writer.starts=1 /\ Writer.outcome="unknown" /\ Writer.cut="removed"
    /\ h.successor="done")

\* Projection of the proposed shim lock plan, before any mixed-mode SDK handoff.
\* A legacy reader protects its 4.35 artifacts using the two package locks.
\* Both 4.36 writers read the same package inputs and write the same private roots.
ScopedOps == DOMAIN h.scopedLocks
ScopedSources == {"host-package", "interop-package"}
ScopedOutputs == {"private-host-436", "private-interop-436"}
ScopedPlan(op) ==
  CASE op="legacy-reader" -> [sh |-> ScopedSources, ex |-> {}]
    [] op="broad-clean" ->
       IF Mutation="scoped-clean-sh"
       THEN [sh |-> ScopedSources, ex |-> {}]
       ELSE [sh |-> {}, ex |-> ScopedSources]
    [] OTHER ->
       CASE Mutation="scoped-legacy-ex" -> [sh |-> {}, ex |-> ScopedSources \cup ScopedOutputs]
         [] op="same-output-writer" /\ Mutation="scoped-omit-output" ->
            [sh |-> ScopedSources, ex |-> {}]
         [] OTHER -> [sh |-> ScopedSources, ex |-> ScopedOutputs]
ScopedCompatible(a,b) ==
  LET aPlan == ScopedPlan(a)
      bPlan == ScopedPlan(b)
  IN /\ aPlan.ex \cap (bPlan.sh \cup bPlan.ex) = {}
     /\ bPlan.ex \cap (aPlan.sh \cup aPlan.ex) = {}
ScopedHeld(op) == h.scopedLocks[op]="held"
ScopedAdmit(op) == HostOnly(h.scopedLocks[op]="idle"
  /\ (\A peer \in ScopedOps: ScopedHeld(peer) => ScopedCompatible(op,peer))
  /\ h'=[h EXCEPT !.scopedLocks[op]="held"])
ScopedRelease(op) == HostOnly(ScopedHeld(op)
  /\ h'=[h EXCEPT !.scopedLocks[op]="released"])
ScopedNext == \E op \in ScopedOps: ScopedAdmit(op) \/ ScopedRelease(op)
ScopedLockSpec == HostInit /\ [][ScopedNext]_hv
ScopedWritersExcluded == ~(ScopedHeld("private-writer") /\ ScopedHeld("same-output-writer"))
ScopedCleanExcluded == ScopedHeld("broad-clean") =>
  \A op \in ScopedOps \ {"broad-clean"}: ~ScopedHeld(op)
NoScopedOverlap == ~(ScopedHeld("legacy-reader") /\ ScopedHeld("private-writer"))

=============================================================================
