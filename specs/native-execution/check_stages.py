#!/usr/bin/env python3
"""Bounded message/gate model: two stages, one warm scope, two supervisors.

Unlike the coarse lifecycle model, duplicate/delayed messages are actual queued
values. A receiver validates each delivery. This is still not runtime code.
"""
from collections import deque
from dataclasses import dataclass, replace
import json
from pathlib import Path


@dataclass(frozen=True,order=True)
class Frame:
    kind: str
    stage: int
    owner: int
    generation: int
    destination: int
    intent: int  # accepted intent for stage i is i; unrelated input is rejected


@dataclass(frozen=True)
class State:
    service: bool = True
    epoch: int = 0
    crashed: bool = False
    created: int = 0
    ready: int = 0
    scope_owner: int = -1
    generations: tuple = (0,0)
    bindings: tuple = (None,None)  # immutable accepted stage -> owner/generation
    authorization_epochs: tuple = (-1,-1)
    issued: frozenset = frozenset()
    sent: frozenset = frozenset()  # actual send history, NOT a durable receipt
    messages: tuple = ()
    consumed: frozenset = frozenset()
    sealed: frozenset = frozenset()
    pending: tuple = (None,None)  # actual supervisor-local launch gate contents
    live: tuple = (None,None)     # actual command access, not a database phase
    cancelling: tuple = (None,None)  # request to stop, NOT observed process exit
    starts: tuple = (0,0)
    spawn_calls: tuple = (0,0)
    finished: int = 0             # actual stage scope stopped, including failures
    terminal: int = 0             # service received complete stage closure evidence
    retained: int = 0             # result data is separate from the consumed gate
    idle: bool = True
    redeliveries: int = 0
    replaced: bool = False
    cross_cancel: bool = False
    bad_launch: bool = False


@dataclass(frozen=True)
class Policy:
    name: str
    controls: bool = False
    max_redeliveries: int = 1
    forget_old_gate: bool = False
    replay_failed_spawn: bool = False
    rebind_after_restart: bool = False
    ignore_owner: bool = False
    ignore_generation: bool = False
    old_cancel_targets_current: bool = False
    old_result_sets_idle: bool = False


def tup(t,i,value):
    out=list(t)
    out[i]=value
    return tuple(out)


def token(frame):
    return frame.stage,frame.owner,frame.generation


def append(s,frame):
    return tuple(sorted(s.messages+(frame,)))


def steps(s,p):
    if s.service and not s.crashed:
        yield "service crashes (local pending GO/spawn survives)",replace(s,service=False,crashed=True)
    if not s.service:
        yield "service restarts; accepted bindings survive",replace(s,service=True,epoch=s.epoch+1)
    if s.service:
        for u in range(2):
            if not s.created & (1<<u):
                yield f"create supervisor {u} (duplicate scope process possible)",replace(s,created=s.created|(1<<u))
            if s.created & (1<<u) and not s.ready & (1<<u):
                yield f"supervisor {u} Ready reply",replace(s,ready=s.ready|(1<<u))
            if s.scope_owner<0 and s.ready & (1<<u):
                yield f"persist sole warm-scope binding to supervisor {u}",replace(s,scope_owner=u)
        u=s.scope_owner
        if u>=0:
            for i in range(2):
                if s.bindings[i] is None and s.idle and (i==0 or s.terminal & 1):
                    key=(i,u,s.generations[u])
                    consumed=s.consumed
                    sealed=s.sealed
                    if p.forget_old_gate and i==1:
                        consumed=frozenset(k for k in consumed if k[0]!=0)
                        sealed=frozenset(k for k in sealed if k[0]!=0)
                    yield f"START({i+1}) commits binding/authorization only",replace(s,
                        bindings=tup(s.bindings,i,(u,s.generations[u])),issued=s.issued|{key},
                        authorization_epochs=tup(s.authorization_epochs,i,s.epoch),
                        idle=False,consumed=consumed,sealed=sealed)
            for key in s.issued:
                i,owner,g=key
                if (key not in s.sent and s.bindings[i]==(owner,g)
                        and s.authorization_epochs[i]==s.epoch and not s.terminal & (1<<i)):
                    # Network enqueue is a separate event. If the service dies
                    # after this point, delivery may still happen. If it dies
                    # before this point, restart never sends the old authority.
                    frame=Frame("go",i,owner,g,owner,i)
                    yield f"enqueue first GO({i+1}) separately from binding",replace(s,
                        sent=s.sent|{key},messages=append(s,frame))
            if (p.rebind_after_restart and s.epoch and s.bindings[0] is not None
                    and not s.terminal & 1):
                other=1-s.bindings[0][0]
                key=(0,other,s.generations[other])
                if s.ready & (1<<other) and key not in s.issued:
                    frame=Frame("go",*key,other,0)
                    yield "BUG: restart reauthorizes same stage to duplicate supervisor",replace(s,
                        issued=s.issued|{key},sent=s.sent|{key},messages=append(s,frame))
        # Result expiry changes result retention, never accepted intent or gate.
        for i in range(2):
            if s.retained & (1<<i):
                yield f"ACK/expiry removes stage {i+1} result data only",replace(s,retained=s.retained&~(1<<i))
            if (s.finished & (1<<i) and not s.terminal & (1<<i)
                    and all(f is None or f.stage!=i for f in s.pending+s.live)):
                # Explicit complete-scope observation; a mere old result message
                # is not this event. A stopped stage may still lack proof output.
                yield f"observe stage {i+1} stopped; result may be unavailable",replace(s,
                    terminal=s.terminal|(1<<i),retained=s.retained|(1<<i),idle=True)
        if (s.terminal==3 and not s.replaced and s.scope_owner>=0
                and all(f is None for f in s.pending+s.live)):
            u=s.scope_owner
            # An explicitly NEW generation after complete old scope closure.
            # Persisted stage bindings remain historical, not runnable new work.
            yield "replace idle warm generation after both stages closed",replace(s,
                generations=tup(s.generations,u,s.generations[u]+1),
                consumed=frozenset(k for k in s.consumed if k[1]!=u),
                sealed=frozenset(k for k in s.sealed if k[1]!=u),replaced=True)

    # Re-delivery is independent of service life and earlier queue consumption.
    # It can happen between consume and spawn, or after stage/generation change.
    if s.redeliveries < p.max_redeliveries:
        # Both explicit jobs are accepted before this model starts. These are
        # retry inputs, including changed intent; they pass through a real
        # receiver transition instead of an unconditional retry stutter.
        for i in range(2):
            for intent in (i,1-i):
                frame=Frame("start",i,-1,-1,-1,intent)
                yield f"client sends START retry({i+1}, intent={intent})",replace(s,
                    messages=append(s,frame),redeliveries=s.redeliveries+1)
        for key in s.issued:
            i,u,g=key
            if key in s.sent:
                for destination in range(2):
                    frame=Frame("go",i,u,g,destination,i)
                    yield f"network duplicates/delays GO({i+1}) to supervisor {destination}",replace(s,
                        messages=append(s,frame),redeliveries=s.redeliveries+1)
            if p.controls:
                for kind in ("cancel","result","ack"):
                    if kind in ("result","ack") and not s.finished & (1<<i):
                        continue  # Delayed output must have a causally prior finish.
                    frame=Frame(kind,i,u,g,u,i)
                    yield f"delayed {kind}({i+1}) enters channel",replace(s,
                        messages=append(s,frame),redeliveries=s.redeliveries+1)

    # Messages are not forced FIFO: they may be delayed/reordered across retries
    # and reconnects. Each receive action actually validates the envelope.
    for at,frame in enumerate(s.messages):
        remaining=s.messages[:at]+s.messages[at+1:]
        if frame.kind=="start":
            if not s.service:
                continue
            base=replace(s,messages=remaining)
            if frame.intent!=frame.stage:
                yield f"START({frame.stage+1}) receiver rejects changed immutable intent",base
            elif s.bindings[frame.stage] is not None:
                yield f"START({frame.stage+1}) receiver returns same binding/result",base
            else:
                yield f"START({frame.stage+1}) receiver finds same accepted queued stage",base
            continue
        u=frame.destination
        if not s.created & (1<<u):
            continue
        key=token(frame)
        valid=(key in s.issued and frame.intent==frame.stage
               and (p.ignore_owner or frame.owner==u)
               and (p.ignore_generation or frame.generation==s.generations[u]))
        base=replace(s,messages=remaining)
        context=(" while stage 2 active" if frame.stage==0 and any(
            f is not None and f.stage==1 for f in s.pending+s.live) else "")
        if not valid:
            reasons=[]
            if frame.owner!=u:
                reasons.append("wrong_owner")
            if frame.generation!=s.generations[u]:
                reasons.append("stale_generation")
            if frame.intent!=frame.stage:
                reasons.append("wrong_intent")
            yield f"supervisor {u} rejects mismatched envelope ({','.join(reasons)})",base
            continue
        if frame.kind=="go":
            local_key=(frame.stage,u,s.generations[u])
            if (local_key in s.consumed or local_key in s.sealed
                    or s.pending[u] is not None or s.live[u] is not None):
                yield f"supervisor {u} rejects duplicate/busy GO({frame.stage+1})",base
            else:
                yield f"supervisor {u} consumes one-use GO({frame.stage+1}) BEFORE spawn",replace(base,
                    consumed=s.consumed|{local_key},pending=tup(s.pending,u,frame))
        elif frame.kind=="cancel":
            targets=[f for f in (s.pending[u],s.live[u]) if f is not None]
            matching=[f for f in targets if f.stage==frame.stage]
            if p.old_cancel_targets_current:
                matching=targets
            if matching:
                f=matching[0]
                yield f"supervisor {u} records cancel({frame.stage+1}) request"+context,replace(base,
                    cancelling=tup(s.cancelling,u,f),
                    cross_cancel=s.cross_cancel or f.stage!=frame.stage)
            else:
                local_key=(frame.stage,u,s.generations[u])
                if local_key not in s.consumed and local_key not in s.sealed:
                    # Cancel can arrive before GO. Seal the authenticated local
                    # gate; no process has launched and late GO must reject.
                    yield f"cancel({frame.stage+1}) seals never-consumed gate before GO",replace(base,
                        sealed=s.sealed|{local_key},finished=s.finished|(1<<frame.stage))
                else:
                    yield f"old cancel({frame.stage+1}) does not affect another stage"+context,base
        elif frame.kind=="result":
            # A delayed old result may update its own history, not active-stage
            # custody or the independent observation that the scope is idle.
            yield f"receive old result({frame.stage+1})"+context,replace(base,
                retained=s.retained|(1<<frame.stage),
                idle=True if p.old_result_sets_idle else s.idle)
        else:
            yield f"receive old ACK({frame.stage+1})"+context,replace(base,
                retained=s.retained&~(1<<frame.stage))

    for u,frame in enumerate(s.pending):
        if frame is None:
            continue
        i=frame.stage
        counts=tup(s.spawn_calls,i,s.spawn_calls[i]+1)
        if counts[i]>2:
            continue
        yield f"supervisor {u} spawns stage {i+1}",replace(s,
            pending=tup(s.pending,u,None),live=tup(s.live,u,frame),
            spawn_calls=counts,starts=tup(s.starts,i,s.starts[i]+1),
            bad_launch=s.bad_launch or frame.owner!=u or frame.generation!=s.generations[u]
                       or frame.intent!=i or token(frame) not in s.sent)
        sealed=s.sealed|{(i,u,s.generations[u])}
        consumed=s.consumed
        if p.replay_failed_spawn:
            sealed=sealed-{(i,u,s.generations[u])}
            consumed=consumed-{(i,u,s.generations[u])}
        yield f"supervisor {u} observes spawn failure; seal stage {i+1}",replace(s,
            pending=tup(s.pending,u,None),spawn_calls=counts,finished=s.finished|(1<<i),
            cancelling=tup(s.cancelling,u,None),
            consumed=frozenset(consumed),sealed=frozenset(sealed))
    for u,frame in enumerate(s.live):
        if frame is not None:
            key=(frame.stage,u,s.generations[u])
            yield f"stage {frame.stage+1} full scope exits on supervisor {u}",replace(s,
                live=tup(s.live,u,None),sealed=s.sealed|{key},finished=s.finished|(1<<frame.stage),
                cancelling=tup(s.cancelling,u,None))
    for u,frame in enumerate(s.cancelling):
        if frame is not None:
            key=(frame.stage,u,s.generations[u])
            # A separate identity-safe stop AND full-exit observation; no signal
            # or cancel reply by itself enables the next stage. Spawn can still
            # race a prior cancel request until this observation seals the gate.
            yield f"supervisor {u} observes cancelled stage {frame.stage+1} fully stopped",replace(s,
                pending=tup(s.pending,u,None),live=tup(s.live,u,None),
                cancelling=tup(s.cancelling,u,None),sealed=s.sealed|{key},
                finished=s.finished|(1<<frame.stage))


def violations(s):
    bad=[]
    if max(s.starts)>1:
        bad.append("AtMostOnePayloadLaunchPerStage")
    if max(s.spawn_calls)>1:
        bad.append("OneConsumedGatePerSpawnAttempt")
    if sum(f is not None for f in s.live+s.pending)>1:
        bad.append("OneActiveStagePerWarmScope")
    if s.cross_cancel:
        bad.append("OldControlCannotCancelDifferentStage")
    if s.bad_launch:
        bad.append("PayloadMatchesOwnerGenerationAndIntent")
    if s.idle and any(f is not None for f in s.live+s.pending):
        bad.append("IdleObservationIsNotOldResult")
    return bad


def describe(s):
    return dict(service=s.service,epoch=s.epoch,created=s.created,ready=s.ready,
                scope_owner=s.scope_owner,generations=s.generations,bindings=s.bindings,
                authorization_epochs=s.authorization_epochs,issued=sorted(s.issued),sent=sorted(s.sent),
                consumed=sorted(s.consumed),sealed=sorted(s.sealed),
                starts=s.starts,spawn_calls=s.spawn_calls,finished=s.finished,terminal=s.terminal,
                pending=[None if f is None else f.__dict__ for f in s.pending],
                live=[None if f is None else f.__dict__ for f in s.live],idle=s.idle,
                cancelling=[None if f is None else f.__dict__ for f in s.cancelling],
                messages=[f.__dict__ for f in s.messages],redeliveries=s.redeliveries)


def explore(p,limit=500000,initial=None):
    initial=State() if initial is None else initial
    pred={initial:None}
    todo=deque([initial])
    edges=0
    coverage=set()
    while todo:
        s=todo.popleft()
        if s.created==3 and s.bindings==(None,None):
            coverage.add("two supervisors can exist before a stage binding")
        if any(s.bindings[i] is not None and not any(k[0]==i for k in s.sent)
               for i in range(2)):
            coverage.add("binding persisted before initial GO send")
            if s.crashed:
                coverage.add("service crash between binding and GO send")
        bad=violations(s)
        if bad:
            trace=[]
            cur=s
            while pred[cur] is not None:
                old,label=pred[cur]
                trace.append(dict(action=label,state=describe(cur)))
                cur=old
            return dict(name=p.name,status="counterexample",states=len(pred),transitions=edges,
                        violations=bad,trace=list(reversed(trace)))
        for label,nxt in steps(s,p):
            edges+=1
            if label.startswith("network duplicates"):
                coverage.add("GO redelivery actually enabled")
            if "rejects duplicate" in label:
                coverage.add("duplicate GO reaches receiver and is rejected")
            if "rejects mismatched" in label:
                coverage.add("misdirected/stale GO reaches receiver and is rejected")
            if label.startswith("START") and "receiver returns" in label:
                coverage.add("duplicate START handled")
                if s.terminal and not s.retained:
                    coverage.add("duplicate START after result expiry handled")
            if label.startswith("START") and "rejects changed" in label:
                coverage.add("changed-intent START reached receiver and was rejected")
            if label.startswith("replace idle"):
                coverage.add("warm generation replaced")
            if s.terminal==3 and s.starts==(1,1):
                coverage.add("both explicit stage payloads ran and stopped on warm scope")
            if "old cancel" in label:
                coverage.add("old cancel rejected for inactive stage")
            if "stale_generation" in label and "wrong_owner" not in label:
                coverage.add("stale-generation GO delivered to matching owner slot")
            if "while stage 2 active" in label:
                coverage.add("stage1 control received while stage2 is active")
            if nxt not in pred:
                pred[nxt]=(s,label)
                todo.append(nxt)
                if len(pred)>limit:
                    return dict(name=p.name,status="INCOMPLETE state limit",states=len(pred),
                                transitions=edges,coverage=sorted(coverage))
    return dict(name=p.name,status="bounded safety passed",states=len(pred),transitions=edges,
                coverage=sorted(coverage))


def find_trace(p,predicate,limit=500000):
    """Targeted reachability, continuing past a mutant's first violated rule."""
    initial=State()
    pred={initial:None}
    todo=deque([initial])
    while todo:
        s=todo.popleft()
        if predicate(s):
            trace=[]
            while pred[s] is not None:
                old,label=pred[s]
                trace.append(dict(action=label,state=describe(s)))
                s=old
            return dict(status="witness found",states=len(pred),trace=list(reversed(trace)))
        for label,nxt in steps(s,p):
            if nxt not in pred:
                pred[nxt]=(s,label)
                todo.append(nxt)
                if len(pred)>limit:
                    return dict(status="INCOMPLETE state limit",states=len(pred))
    return dict(status="unreachable in bounded graph",states=len(pred))


def main():
    base=Policy("two stages, duplicate supervisors and delayed GO")
    cases=[base,replace(base,name="late stage control messages",controls=True),
           replace(base,name="BUG forget stage1 gate at stage2",forget_old_gate=True),
           replace(base,name="BUG clear gate after spawn failure",replay_failed_spawn=True),
           replace(base,name="BUG rebind uncertain stage on restart",rebind_after_restart=True),
           replace(base,name="BUG ignore supervisor owner token",ignore_owner=True),
           replace(base,name="BUG ignore scope generation",ignore_generation=True),
           replace(base,name="BUG old cancel targets current stage",controls=True,old_cancel_targets_current=True),
           replace(base,name="BUG old result declares scope idle",controls=True,old_result_sets_idle=True)]
    results=[]
    for policy in cases:
        result=explore(policy)
        results.append(result)
        print(policy.name,result["status"],result["states"],flush=True)
    duplicate=find_trace(cases[4],lambda s:max(s.starts)>1)
    Path("stage-results.json").write_text(json.dumps(
        {"safety":results,"mutant_rebind_actual_duplicate_launch":duplicate},indent=2)+"\n")
    expected=["bounded safety passed"]*2+["counterexample"]*7
    if [r["status"] for r in results]!=expected:
        raise SystemExit("Unexpected stage result: inspect incomplete/failed model")
    if duplicate["status"]!="witness found":
        raise SystemExit("Rebind negative control did not reach actual duplicate payload launch")


if __name__=="__main__":
    main()
