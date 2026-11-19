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
  hostClosure |-> <<>>, staleSeen |-> FALSE, duplicateSeen |-> FALSE]
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
Send(i) == HostOnly(ReadyFor(i) /\ h.cmd[i]="idle"
  /\ h'=[h EXCEPT !.cmd[i]="sent", !.sentKey[i]=Expected(i)])
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
=============================================================================
