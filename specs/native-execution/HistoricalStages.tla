------------------------------ MODULE HistoricalStages ------------------------------
EXTENDS Naturals, Sequences, FiniteSets, TLC

\* Two accepted immutable stages and at most two supervisor incarnations.
\* Not an OS, SQLite, authenticated-channel or eventual-recovery proof.
CONSTANTS MaxRedeliveries, Controls, Scenario, LateKindA, LateKindB, Mutant
Stages == {1,2}
Owners == {1,2}
None == <<>>
StartKind == 0
GoKind == 1
CancelKind == 2
ResultKind == 3
AckKind == 4
CleanupKind == 5
Late == Scenario \in {"LatePending","LateRunning","LateReplaced"}
Frame(k,i,u,g,d,t) == <<k,i,u,g,d,t>>
Key(f) == <<f[2],f[3],f[4]>>
LocalKey(f) == <<f[2],f[5],f[4]>>
FrameOrder(f) == (((((f[1]*3+f[2])*3+f[3])*2+f[4])*3+f[5])*3+f[6])
RECURSIVE Insert(_,_)
Insert(q,f) == IF Len(q)=0 THEN <<f>>
               ELSE IF FrameOrder(f)<=FrameOrder(Head(q)) THEN <<f>> \o q
               ELSE <<Head(q)>> \o Insert(Tail(q),f)
Remove(q,a) == SubSeq(q,1,a-1) \o SubSeq(q,a+1,Len(q))
EmptyOwners == [u \in Owners |-> None]
EmptyBindings == [i \in Stages |-> None]
ZeroCounts == [i \in Stages |-> 0]
K1 == <<1,1,0>>
K2 == <<2,1,0>>
GO1 == Frame(GoKind,1,1,0,1,1)
GO2 == Frame(GoKind,2,1,0,1,2)

VARIABLE s
vars == <<s>>
NewState == [service |-> TRUE, epoch |-> 0, crashed |-> FALSE,
 created |-> {}, ready |-> {}, scopeOwner |-> 0,
 generations |-> [u \in Owners |-> 0], bindings |-> EmptyBindings,
 acceptedIntent |-> [i \in Stages |-> i], startReply |-> None,
 authEpoch |-> [i \in Stages |-> 2], issued |-> {}, sent |-> {},
 messages |-> <<>>, consumed |-> {}, sealed |-> {},
 pending |-> EmptyOwners, live |-> EmptyOwners, cancelling |-> EmptyOwners,
 starts |-> ZeroCounts, spawnCalls |-> ZeroCounts,
 finished |-> {}, terminal |-> {}, publishedResult |-> {}, knownResult |-> {}, retained |-> {},
 acknowledged |-> {}, expired |-> {}, idle |-> TRUE, redeliveries |-> 0,
 replaced |-> FALSE, injected |-> {}, delivered |-> 0,
 observed |-> {},
 badCancel |-> FALSE, badLaunch |-> FALSE, badAck |-> FALSE, badCallback |-> FALSE]
BoundState == [NewState EXCEPT !.created={1}, !.ready={1}, !.scopeOwner=1,
 !.bindings[1]= <<1,0>>, !.authEpoch[1]=0, !.issued={K1}, !.idle=FALSE]
SecondState == [BoundState EXCEPT !.bindings[2]= <<1,0>>, !.authEpoch[2]=0,
 !.issued={K1,K2}, !.sent={K1,K2}, !.consumed={K1,K2}, !.sealed={K1},
 !.starts[1]=1, !.spawnCalls[1]=1, !.finished={1}, !.terminal={1},
 !.publishedResult={1}, !.knownResult={1}, !.retained={1}]
Initial == CASE Scenario="Full" -> NewState
 [] Scenario="Bound" -> BoundState
 [] Scenario="CrashBeforeSend" -> [BoundState EXCEPT !.service=FALSE, !.crashed=TRUE]
 [] Scenario="CrashAfterSend" -> [BoundState EXCEPT !.service=FALSE, !.crashed=TRUE,
                                    !.sent={K1}, !.messages= <<GO1>>]
 [] Scenario="CrashAfterConsume" -> [BoundState EXCEPT !.service=FALSE, !.crashed=TRUE,
                    !.sent={K1}, !.consumed={K1}, !.pending[1]=GO1]
 [] Scenario="WrongGoIntent" -> [BoundState EXCEPT !.sent={K1},
                                      !.messages= <<Frame(GoKind,1,1,0,1,2)>>]
 [] Scenario="LatePending" -> [SecondState EXCEPT !.pending[1]=GO2]
 [] Scenario="LateRunning" -> [SecondState EXCEPT !.live[1]=GO2,
                                           !.starts[2]=1, !.spawnCalls[2]=1]
 [] Scenario="LateReplaced" -> [SecondState EXCEPT !.generations[1]=1,
       !.consumed={}, !.sealed={}, !.starts[2]=1, !.spawnCalls[2]=1,
       !.finished=Stages, !.terminal=Stages, !.publishedResult=Stages, !.knownResult=Stages,
       !.retained=Stages, !.idle=TRUE, !.replaced=TRUE]
Init == s=Initial
Active(i) == \E u \in Owners:
                 (s.pending[u]#None /\ s.pending[u][2]=i)
                 \/ (s.live[u]#None /\ s.live[u][2]=i)
NoActive == \A u \in Owners: s.pending[u]=None /\ s.live[u]=None
HasOtherStage(i) == \E j \in Stages\{i}: Active(j)
AllKeys == Stages \X Owners \X {0,1}

Crash == /\ s.service /\ ~s.crashed
         /\ s'=[s EXCEPT !.service=FALSE, !.crashed=TRUE]
Restart == /\ ~s.service /\ s'=[s EXCEPT !.service=TRUE, !.epoch=@+1]
Create(u) == /\ ~Late /\ s.service /\ u \notin s.created
             /\ s'=[s EXCEPT !.created=@\cup{u}]
Ready(u) == /\ s.service /\ u \in s.created\s.ready
            /\ s'=[s EXCEPT !.ready=@\cup{u}]
BindScope(u) == /\ s.service /\ s.scopeOwner=0 /\ u \in s.ready
                /\ s'=[s EXCEPT !.scopeOwner=u]
Bind(i) == LET u==s.scopeOwner IN
 /\ s.service /\ u \in Owners /\ s.bindings[i]=None /\ s.idle
 /\ (i=1 \/ 1 \in s.terminal)
 /\ s'=[s EXCEPT !.bindings[i]= <<u,s.generations[u]>>, !.authEpoch[i]=s.epoch,
       !.issued=@\cup{<<i,u,s.generations[u]>>}, !.idle=FALSE,
       !.consumed=IF Mutant="ForgetGate" /\ i=2 THEN {k \in @: k[1]#1} ELSE @,
       !.sealed=IF Mutant="ForgetGate" /\ i=2 THEN {k \in @: k[1]#1} ELSE @]
FirstGo(k) == LET i==k[1] u==k[2] g==k[3] IN
 /\ s.service /\ k \in s.issued\s.sent /\ s.bindings[i]= <<u,g>>
 /\ s.authEpoch[i]=s.epoch /\ i \notin s.terminal
 /\ s'=[s EXCEPT !.sent=@\cup{k}, !.messages=Insert(@,Frame(GoKind,i,u,g,u,i))]
BadRebind == LET u==IF s.bindings[1][1]=1 THEN 2 ELSE 1
                k== <<1,u,s.generations[u]>> IN
 /\ Mutant="Rebind" /\ s.service /\ s.epoch=1 /\ s.bindings[1]#None
 /\ 1 \notin s.terminal /\ u \in s.ready /\ k \notin s.issued
 /\ s'=[s EXCEPT !.issued=@\cup{k}, !.sent=@\cup{k},
                  !.messages=Insert(@,Frame(GoKind,1,u,s.generations[u],u,1))]
Expire(i) == /\ s.service /\ i \in s.retained
             /\ s'=[s EXCEPT !.retained=@\{i}, !.expired=@\cup{i}]
Observe(i) == /\ s.service /\ i \in s.finished\s.terminal /\ ~Active(i)
 /\ s'=[s EXCEPT !.terminal=@\cup{i}, !.idle=TRUE,
       !.knownResult=IF Mutant="ClosureInventsResult" THEN @\cup{i} ELSE @,
       !.retained=IF Mutant="ClosureInventsResult" THEN @\cup{i} ELSE @]
FirstResult(i) == LET b==s.bindings[i] IN
 /\ i \in s.finished\s.publishedResult /\ b#None
 /\ s'=[s EXCEPT !.publishedResult=@\cup{i},
                 !.messages=Insert(@,Frame(ResultKind,i,b[1],b[2],b[1],i))]
Replace == /\ s.service /\ s.terminal=Stages /\ ~s.replaced
 /\ s.scopeOwner \in Owners /\ NoActive
 /\ s'=[s EXCEPT !.generations[s.scopeOwner]=@+1, !.replaced=TRUE,
       !.consumed={k \in @: k[2]#s.scopeOwner}, !.sealed={k \in @: k[2]#s.scopeOwner}]

Enqueue(f) == /\ s.redeliveries<MaxRedeliveries
 /\ s'=[s EXCEPT !.messages=Insert(@,f), !.redeliveries=@+1]
Retry(i,t) == /\ ~Late /\ Enqueue(Frame(StartKind,i,0,0,0,t))
DuplicateGo(k,d) == /\ ~Late /\ k \in s.sent
                   /\ Enqueue(Frame(GoKind,k[1],k[2],k[3],d,k[1]))
OldControl(k,kind) == /\ ~Late /\ Controls /\ k \in s.issued
 /\ kind \in {CancelKind,ResultKind,AckKind}
 /\ (kind=CancelKind \/ (kind=ResultKind /\ k[1] \in s.publishedResult)
                     \/ (kind=AckKind /\ k[1] \in s.knownResult))
 /\ Enqueue(Frame(kind,k[1],k[2],k[3],k[2],k[1]))
InjectLate(slot) == LET kind==IF slot=1 THEN LateKindA ELSE LateKindB IN
 /\ Late /\ slot \notin s.injected
 /\ s'=[s EXCEPT !.injected=@\cup{slot},
                 !.messages=Insert(@,Frame(kind,1,1,0,1,1))]

Receive(a) == LET f==s.messages[a] kind==f[1] i==f[2] u==f[5]
 k==Key(f) lk== <<i,u,s.generations[u]>>
 basic==k \in s.issued /\ (Mutant="IgnoreIntent" \/ f[6]=s.acceptedIntent[i])
                      /\ (Mutant="IgnoreOwner" \/ f[3]=u)
 current==Mutant="IgnoreGeneration" \/ f[4]=s.generations[u]
 history==s.bindings[i]= <<f[3],f[4]>> /\ f[3]=u
 base==[s EXCEPT !.messages=Remove(@,a),
      !.delivered=IF Late /\ i=1 THEN @+1 ELSE @,
      !.observed=@
        \cup (IF Late /\ i=1 /\ HasOtherStage(1) THEN {"activeSecond"} ELSE {})
        \cup (IF Late /\ i=1 /\ s.epoch=1 THEN {"afterRestart"} ELSE {})
        \cup (IF Late /\ i=1 /\ s.replaced THEN {"afterReplacement"} ELSE {})
        \cup (IF Late /\ i=1 /\ kind=CleanupKind THEN {"cleanupReceived"} ELSE {})]
 target==IF s.pending[u]#None THEN s.pending[u] ELSE s.live[u]
 IN
 /\ a \in 1..Len(s.messages)
 /\ IF kind=StartKind THEN
       /\ s.service
       /\ s'=[base EXCEPT !.startReply= <<i,f[6],
          IF f[6]#s.acceptedIntent[i] /\ Mutant#"AcceptChangedStart" THEN "rejected"
          ELSE IF s.bindings[i]=None THEN "queued" ELSE "sameBinding">>]
    ELSE /\ u \in s.created
         /\ (kind \notin {ResultKind,AckKind,CleanupKind} \/ s.service)
         /\ IF ~basic \/ (~current /\ kind \notin {ResultKind,AckKind})
               \/ (kind \in {ResultKind,AckKind} /\ ~history) THEN
                 IF kind=CleanupKind /\ Mutant="CallbackGeneration" /\ ~current
                 THEN s'=[base EXCEPT !.idle=FALSE, !.badCallback=TRUE]
                 ELSE s'=base
            ELSE CASE kind=GoKind ->
              IF lk \in s.consumed\cup s.sealed \/ target#None THEN s'=base
              ELSE s'=[base EXCEPT !.consumed=@\cup{lk}, !.pending[u]=f]
            [] kind=CancelKind ->
              IF target#None /\ (target[2]=i \/ Mutant="CancelCurrent") THEN
                s'=[base EXCEPT !.cancelling[u]=target,
                     !.badCancel=@ \/ target[2]#i]
              ELSE IF lk \notin s.consumed\cup s.sealed THEN
                s'=[base EXCEPT !.sealed=@\cup{lk}, !.finished=@\cup{i}]
              ELSE s'=base
            [] kind=ResultKind ->
              s'=[base EXCEPT !.knownResult=@\cup{i},
                    !.retained=IF i \notin s.acknowledged\cup s.expired
                               THEN @\cup{i} ELSE @,
                    !.idle=IF Mutant="ResultIdle" THEN TRUE ELSE @]
            [] kind=AckKind ->
              IF i \notin s.knownResult THEN s'=base ELSE
              s'=[base EXCEPT !.retained=IF Mutant="AckCurrent" THEN @\{i,2} ELSE @\{i},
                    !.acknowledged=@\cup{i},
                    !.badAck=@ \/ (Mutant="AckCurrent" /\ i#2 /\ 2 \in s.retained)]
            [] kind=CleanupKind ->
              IF Mutant="CallbackCurrent" /\ HasOtherStage(i) THEN
                s'=[base EXCEPT !.pending=EmptyOwners, !.live=EmptyOwners,
                               !.idle=TRUE, !.badCallback=TRUE]
              ELSE s'=base

Spawn(u) == LET f==s.pending[u] i==f[2] IN
 /\ f#None /\ s.spawnCalls[i]<2
 /\ s'=[s EXCEPT !.pending[u]=None, !.live[u]=f,
       !.spawnCalls[i]=@+1, !.starts[i]=@+1,
       !.badLaunch=@ \/ f[3]#u \/ f[4]#s.generations[u]
                    \/ f[6]#i \/ Key(f) \notin s.sent]
SpawnFailed(u) == LET f==s.pending[u] i==f[2] k== <<i,u,s.generations[u]>> IN
 /\ f#None /\ s.spawnCalls[i]<2
 /\ s'=[s EXCEPT !.pending[u]=None, !.cancelling[u]=None,
       !.spawnCalls[i]=@+1, !.finished=@\cup{i},
       !.consumed=IF Mutant="ReopenFailedGate" THEN @\{k} ELSE @,
       !.sealed=IF Mutant="ReopenFailedGate" THEN @\{k} ELSE @\cup{k}]
Exit(u) == LET f==s.live[u] i==f[2] IN
 /\ f#None /\ s'=[s EXCEPT !.live[u]=None, !.cancelling[u]=None,
                 !.sealed=@\cup{<<i,u,s.generations[u]>>}, !.finished=@\cup{i}]
Stopped(u) == LET f==s.cancelling[u] i==f[2] IN
 /\ f#None /\ s'=[s EXCEPT !.pending[u]=None, !.live[u]=None,
       !.cancelling[u]=None, !.sealed=@\cup{<<i,u,s.generations[u]>>}, !.finished=@\cup{i}]

Next == Crash \/ Restart \/ Replace \/ BadRebind
 \/ (\E u \in Owners: Create(u) \/ Ready(u) \/ BindScope(u) \/ Spawn(u) \/ SpawnFailed(u) \/ Exit(u) \/ Stopped(u))
 \/ (\E i \in Stages: Bind(i) \/ Expire(i) \/ Observe(i) \/ FirstResult(i)
                        \/ (\E t \in Stages: Retry(i,t)))
 \/ (\E k \in AllKeys: FirstGo(k) \/ (\E d \in Owners: DuplicateGo(k,d))
                     \/ (\E kind \in {CancelKind,ResultKind,AckKind}: OldControl(k,kind)))
 \/ (\E a \in 1..Len(s.messages): Receive(a))
 \/ (\E slot \in {1,2}: InjectLate(slot))
Spec == Init /\ [][Next]_vars

TypeOK == /\ s.service \in BOOLEAN /\ s.crashed \in BOOLEAN /\ s.epoch \in 0..1
          /\ s.created \subseteq Owners /\ s.ready \subseteq s.created
          /\ s.scopeOwner \in Owners\cup{0}
          /\ s.generations \in [Owners -> 0..1]
          /\ s.authEpoch \in [Stages -> 0..2]
          /\ s.issued \subseteq AllKeys /\ s.sent \subseteq s.issued
          /\ s.consumed \subseteq AllKeys /\ s.sealed \subseteq AllKeys
          /\ s.starts \in [Stages -> 0..2] /\ s.spawnCalls \in [Stages -> 0..2]
          /\ s.finished \subseteq Stages /\ s.terminal \subseteq s.finished
          /\ s.publishedResult \subseteq Stages /\ s.knownResult \subseteq Stages
          /\ s.retained \subseteq s.knownResult
          /\ s.acknowledged \subseteq s.knownResult /\ s.expired \subseteq s.knownResult
          /\ s.redeliveries \in 0..MaxRedeliveries /\ s.delivered \in 0..2
          /\ s.observed \subseteq {"activeSecond","afterRestart","afterReplacement","cleanupReceived"}
UniqueStageAuthorization == \A i \in Stages: Cardinality({k \in s.issued:k[1]=i})<=1
AcceptedIntentImmutable == s.acceptedIntent=[i \in Stages |-> i]
ChangedStartIntentRejected == s.startReply=None \/
  (s.startReply[2]=s.acceptedIntent[s.startReply[1]] \/ s.startReply[3]="rejected")
KnownResultWasPublished == s.knownResult \subseteq s.publishedResult
AtMostOneLaunch == \A i \in Stages: s.starts[i]<=1
OneConsumedGatePerSpawnAttempt == \A i \in Stages: s.spawnCalls[i]<=1
OneActiveStage == Cardinality({u \in Owners: s.pending[u]#None})
                   + Cardinality({u \in Owners: s.live[u]#None})<=1
CorrectLaunchBinding == ~s.badLaunch
OldCancelIsolated == ~s.badCancel
OldAckIsolated == ~s.badAck
OldCleanupIsolated == ~s.badCallback
IdleMeansNoActive == s.idle => NoActive
ExpiredResultsStayAbsent == s.retained \cap (s.acknowledged\cup s.expired)={}
NoTwoReceived == s.delivered<2
NoLateReceiveWhileSecondActive == "activeSecond" \notin s.observed
NoLateReceiveAfterRestart == "afterRestart" \notin s.observed
NoLateReceiveAfterReplacement == "afterReplacement" \notin s.observed
NoCleanupReceived == "cleanupReceived" \notin s.observed
NoBothStagesRan == ~(s.starts[1]=1 /\ s.starts[2]=1 /\ s.terminal=Stages)
NoClosedStageWithoutResult == s.terminal \subseteq s.knownResult
NoActualDuplicateLaunch == \A i \in Stages: s.starts[i]<2
=============================================================================
