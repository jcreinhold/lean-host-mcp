#!/usr/bin/env python3
"""Fixed-topology, two-delivery slices; not proof of the combined model.

Callbacks are a NEW abstract proposed API. No owner faults or extra START/GO
traffic are introduced. Service crash/restart and stage exit remain enabled.
"""
from collections import deque
from dataclasses import dataclass,replace
from pathlib import Path
import json
from check_stages import State,Frame,Policy,steps,violations,describe

KINDS=("cancel","ack","result","cleanup")

@dataclass(frozen=True)
class Slice:
    core: State
    injected: int=0
    callbacks: tuple=()
    delivered: int=0
    bad_observation: str=""

def starting(cut):
    keys=frozenset(((0,0,0),(1,0,0)))
    second=Frame("go",1,0,0,0,1)
    base=State(created=1,ready=1,scope_owner=0,bindings=((0,0),(0,0)),
               authorization_epochs=(0,0),issued=keys,sent=keys,consumed=keys,
               sealed=frozenset(((0,0,0),)),starts=(1,0),spawn_calls=(1,0),
               finished=1,terminal=1,retained=1,idle=False)
    if cut=="second pending":
        return Slice(replace(base,pending=(second,None)))
    if cut=="second running":
        return Slice(replace(base,live=(second,None),starts=(1,1),spawn_calls=(1,1)))
    if cut=="generation replaced":
        return Slice(replace(base,generations=(1,0),consumed=frozenset(),sealed=frozenset(),
                             starts=(1,1),spawn_calls=(1,1),finished=3,terminal=3,
                             retained=3,idle=True,replaced=True))
    raise ValueError(cut)

def successors(s,kinds,bug=""):
    for i,kind in enumerate(kinds):
        if not s.injected & (1<<i):
            frame=Frame(kind,0,0,0,0,0)
            if kind=="cleanup":
                yield f"enqueue old cleanup callback #{i+1}",replace(s,
                    injected=s.injected|(1<<i),callbacks=s.callbacks+(frame,))
            else:
                yield f"enqueue old {kind} #{i+1}",replace(s,
                    injected=s.injected|(1<<i),core=replace(s.core,
                        messages=tuple(sorted(s.core.messages+(frame,)))))
    policy=Policy("fixed-topology slice",max_redeliveries=0,
                  old_cancel_targets_current=bug=="cancel current",
                  old_result_sets_idle=bug=="result idle")
    for label,nxt in steps(s.core,policy):
        if label.startswith(("create supervisor","supervisor 1 Ready")):
            continue  # Topology is fixed for these explicit slices.
        received=len(nxt.messages)<len(s.core.messages)
        bad=s.bad_observation
        if received and label.startswith("receive old ACK"):
            if bug=="ack current":
                nxt=replace(nxt,retained=nxt.retained & ~2)
            if (nxt.retained & 2)!=(s.core.retained & 2):
                bad="OldAckMustNotDeleteSecondStageResult"
        yield label,replace(s,core=nxt,delivered=s.delivered+received,bad_observation=bad)
    for i,frame in enumerate(s.callbacks):
        core=s.core
        active=[f for f in core.pending+core.live if f is not None]
        valid=(core.bindings[frame.stage]==(frame.owner,frame.generation)
               and frame.owner==frame.destination
               and frame.generation==core.generations[frame.destination])
        # Callback attests complete closure of THAT stage, already historically
        # closed in these cuts. It says nothing about the current stage.
        label=("reject old cleanup callback: generation/owner mismatch" if not valid
               else "receive old cleanup callback: historical stage only")
        nxt=core
        if bug=="callback current" and active:
            label="BUG old cleanup callback clears current command"
            nxt=replace(core,pending=(None,None),live=(None,None),idle=True)
        if bug=="callback generation" and not valid:
            label="BUG old cleanup callback changes replacement idle state"
            nxt=replace(core,idle=False)
        bad=s.bad_observation
        if ((not valid or any(f.stage!=frame.stage for f in active)) and nxt!=core):
            bad="OldCallbackCannotChangeCurrentStageOrGeneration"
        yield label,replace(s,core=nxt,callbacks=s.callbacks[:i]+s.callbacks[i+1:],
                            delivered=s.delivered+1,bad_observation=bad)

def explore(cut,kinds,bug="",limit=30000):
    initial=starting(cut)
    pred={initial:None}
    todo=deque([initial])
    coverage=set()
    transitions=0
    while todo:
        s=todo.popleft()
        bad=violations(s.core)+([s.bad_observation] if s.bad_observation else [])
        if bad:
            trace=[]
            cur=s
            while pred[cur] is not None:
                old,label=pred[cur]
                trace.append(dict(action=label,core=describe(cur.core),delivered=cur.delivered))
                cur=old
            return dict(status="counterexample",states=len(pred),transitions=transitions,
                        violations=bad,coverage=sorted(coverage),trace=list(reversed(trace)))
        if s.delivered==2:
            coverage.add("both actual deliveries received")
        for label,nxt in successors(s,kinds,bug):
            transitions+=1
            if nxt.delivered>s.delivered:
                if any(f is not None and f.stage==1 for f in s.core.pending+s.core.live):
                    coverage.add("old control delivered while second stage pending/running")
                if s.core.replaced:
                    coverage.add("old control delivered after generation replacement")
                if "callback" in label:
                    coverage.add("cleanup callback actually received")
                if s.core.epoch:
                    coverage.add("old control delivered after service restart")
            if nxt not in pred:
                pred[nxt]=(s,label)
                todo.append(nxt)
                if len(pred)>limit:
                    return dict(status="INCOMPLETE state limit",states=len(pred),
                                transitions=transitions,coverage=sorted(coverage))
    return dict(status="bounded safety passed",states=len(pred),transitions=transitions,
                coverage=sorted(coverage))

def main():
    pairs=[(k,k) for k in KINDS]+[(KINDS[i],KINDS[j])
          for i in range(len(KINDS)) for j in range(i+1,len(KINDS))]
    out={"bound_per_case":30000,"safe":[],"negative_controls":[]}
    for cut in ("second pending","second running","generation replaced"):
        for kinds in pairs:
            result=explore(cut,kinds)
            out["safe"].append(dict(cut=cut,kinds=kinds,**result))
            print(cut,"+".join(kinds),result["status"],result["states"],flush=True)
            assert result["status"]=="bounded safety passed"
            assert "both actual deliveries received" in result["coverage"]
    mutants=[("second running",("cancel","cancel"),"cancel current"),
             ("second running",("result","result"),"result idle"),
             ("second running",("ack","ack"),"ack current"),
             ("second running",("cleanup","cleanup"),"callback current"),
             ("generation replaced",("cleanup","cleanup"),"callback generation")]
    for cut,kinds,bug in mutants:
        result=explore(cut,kinds,bug)
        out["negative_controls"].append(dict(cut=cut,kinds=kinds,bug=bug,**result))
        print("BUG",bug,result["status"],result["states"],flush=True)
        assert result["status"]=="counterexample"
    Path("late-control-results.json").write_text(json.dumps(out,indent=2)+"\n")

if __name__=="__main__":
    main()
