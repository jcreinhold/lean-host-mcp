----------------------------- MODULE Stages -----------------------------
EXTENDS WorkerCustody, FiniteSets
CONSTANT Profile, Fault, Mutation
VARIABLE h
hv == <<h, vars>>
Commands == {1, 2}
Faults == Fault = "explore"
Original == <<1,1,1>>   \* job, logical stage, explicit revision
NativeId == <<"store",1,"nonce">>   \* store identity, submission window, nonce
Scope == <<"worker",1,"owner">>
CommandKey(i) == <<IF Profile="warm" THEN <<i,1,1>> ELSE Original, i, Scope>>
HostInit == Init /\ h = [
  binding |-> <<>>, retry |-> "none", buffered |-> "none", submitted |-> FALSE,
  accepted |-> FALSE, rolledBackPartial |-> FALSE, domains |-> 0, permit |-> "unused",
  online |-> TRUE, crashed |-> FALSE, refused |-> FALSE, cancelled |-> FALSE, fenced |-> FALSE,
  wrongFrame |-> FALSE, readAdmitted |-> FALSE, warmSaved |-> FALSE, cmd |-> [i \in Commands |-> "idle"], starts |-> [i \in Commands |-> 0],
  report |-> "none", primary |-> "none", copied |-> "none", ack |-> FALSE, cleanupError |-> FALSE,
  unrelated |-> "queued", successor |-> "queued"]
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
ReserveScope == h.online /\ h.domains=2 /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced /\ SdkOnly(Reserve)
ConsumePermit == HostOnly(h.online /\ ~h.crashed /\ phase="reserved" /\ h.permit="unused"
  /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced /\ h'=[h EXCEPT !.permit="authorized"])
SendLaunch == HostOnly(h.online /\ ~h.crashed /\ h.permit="authorized" /\ ~h.cancelled /\ ~h.refused /\ ~h.fenced
  /\ h'=[h EXCEPT !.permit="sent"])
ReceiveLaunch == HostOnly(h.permit="sent" /\ ~h.fenced /\ phase="reserved"
  /\ h'=[h EXCEPT !.permit="consumed"])
RefuseSecondDomain == HostOnly(Faults /\ phase="idle" /\ h.domains=1 /\ ~h.refused /\ ~h.fenced
  /\ h'=[h EXCEPT !.refused=TRUE])
NativeLaunchFails == Faults /\ h.permit="consumed" /\ ~h.fenced /\ SpawnFails
  /\ h'=[h EXCEPT !.permit="spent", !.report="not-started"]
NativeLaunch == h.permit="consumed" /\ ~h.fenced /\ Spawn /\ h'=[h EXCEPT !.permit="spent"]
\* The surviving receiver serializes this fence against ReceiveLaunch and NativeLaunch.
\* No-spawn rollback is legal only after it seals the reservation before physical spawn.
Fence == HostOnly((h.cancelled \/ h.refused \/ h.crashed) /\ ~h.fenced /\ supervisorAlive
  /\ h'=[h EXCEPT !.fenced=TRUE])
Rollback == h.fenced /\ phase \in {"idle","reserved"} /\ h.domains>0
  /\ (IF phase="reserved" THEN SpawnFails ELSE UNCHANGED vars)
  /\ h'=[h EXCEPT !.rolledBackPartial=(h.domains=1), !.domains=0]
ReadyFor(i) == phase="running" /\ supervisorAlive /\ ~h.fenced /\ ~h.cancelled
  /\ h.online /\ ~h.crashed /\ ((startupAcked /\ h.readAdmitted) \/ Mutation="early-command")
  /\ (i=1 \/ (h.cmd[1]="valid" /\ (Profile="companion" \/ h.warmSaved)))
Send(i) == HostOnly(ReadyFor(i) /\ h.cmd[i]="idle"
  /\ h'=[h EXCEPT !.cmd[i]="sent"])
Deliver(i,frame) == HostOnly(rootPresent /\ phase="running" /\ supervisorAlive /\ ~h.fenced
  /\ h.cmd[i]="sent" /\ (frame=CommandKey(i) \/ Mutation="current-endpoint")
  /\ h'=[h EXCEPT !.cmd[i]="running", !.starts[i]=@+1, !.wrongFrame=(frame/=CommandKey(i))])
Execute(i) == Deliver(i,CommandKey(i))
LateDelivery == Fault="late" /\ Deliver(2,CommandKey(1))
Reply(i,valid) == HostOnly(rootPresent /\ h.online /\ phase="running" /\ h.cmd[i]="running"
  /\ (valid \/ Faults) /\ h.primary="none"
  /\ h'=[h EXCEPT !.cmd[i]=IF valid THEN "valid" ELSE "invalid",
     !.buffered=IF ~valid THEN IF i=1 THEN "invalid-first" ELSE "invalid-second"
               ELSE IF i=2 THEN "success" ELSE @])
Publish == HostOnly(h.online /\ h.buffered/="none" /\ h.primary="none"
  /\ h'=[h EXCEPT !.primary=h.buffered, !.buffered="none"])
Cancel == HostOnly(Faults /\ h.accepted /\ h.online /\ ~h.cancelled /\ h.primary="none"
  /\ h'=[h EXCEPT !.cancelled=TRUE])
CrashCaller == HostOnly(Faults /\ h.submitted /\ h.online /\ ~h.crashed
  /\ h'=[h EXCEPT !.online=FALSE, !.crashed=TRUE, !.buffered="none"])
Recover == HostOnly(~h.online /\ h'=[h EXCEPT !.online=TRUE])
\* No new execution revision on recovery. Draining the old channel or certified
\* generation retirement settles its ability to execute. Missing reply remains unknown.
SetDisposition == HostOnly(h.accepted /\ h.online /\ h.primary="none" /\ h.report="none"
  /\ h.buffered="none" /\ (h.crashed \/ h.fenced \/ phase \in {"stopping","closed"})
  /\ h'=[h EXCEPT !.report=IF h.permit/="unused" THEN "unknown" ELSE "not-started"])
SaveCopy == HostOnly(h.online /\ h.primary/="none" /\ h.copied="none"
  /\ h'=[h EXCEPT !.copied=h.primary])
AckCopy == HostOnly(h.online /\ (h.copied/="none" \/ (Mutation="early-ack" /\ h.primary/="none")) /\ ~h.ack /\ h'=[h EXCEPT !.ack=TRUE])
SaveWarm == HostOnly(Profile="warm" /\ h.online /\ h.cmd[1]="valid" /\ ~h.warmSaved
  /\ h'=[h EXCEPT !.warmSaved=TRUE])
StopScope == supervisorAlive /\ (h.fenced \/ h.primary/="none") /\ SdkOnly(RequestStop)
CleanupError == HostOnly(Faults /\ phase="stopping" /\ ~h.cleanupError
  /\ h'=[h EXCEPT !.cleanupError=TRUE,
     !.primary=IF Mutation="overwrite-primary" /\ @/="none" THEN "cleanup-error" ELSE @])
ReleaseDomains == HostOnly(phase="closed" /\ h.domains>0 /\ h'=[h EXCEPT !.domains=0])
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
  \/ (\E i \in Commands: Send(i) \/ Execute(i) \/ Reply(i,TRUE))
HostNext == LateDelivery \/ RefuseSecondDomain \/ NativeLaunchFails \/ (\E intent \in {"intent-a","intent-b"}: Retry(intent)) \/ Main \/ Other \/ Loss \/ Cancel \/ CrashCaller \/ CleanupError
  \/ (\E i \in Commands: Reply(i,FALSE))
HostSpec == HostInit /\ [][HostNext]_hv /\ WF_hv(Main) /\ WF_hv(Other)
\* Main has finitely many non-stuttering transitions. Faults are bounded and never fair.
SafeCommands == ~h.wrongFrame /\ (\A i \in Commands: h.starts[i]<=1 /\ (h.starts[i]>0 => (startupAcked /\ h.readAdmitted)))
  /\ (h.starts[2]>0 => h.cmd[1]="valid")
  /\ ((Profile="warm" /\ h.starts[2]>0) => h.warmSaved)
  /\ (h.primary="success" => h.cmd[2]="valid")
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
=============================================================================
