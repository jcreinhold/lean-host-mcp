#!/usr/bin/env python3
"""Reduced pending-GO/owner-loss/replacement model; no native processes.

The old accepted stage is 1, an explicitly new stage is 2. Closure observation
is an explicit abstract capability. Missing results are not closure evidence.
"""
from collections import deque
from dataclasses import dataclass,replace,asdict
from pathlib import Path
import json

SUP,GUARD=1,2


@dataclass(frozen=True)
class State:
    service: bool = True
    epoch: int = 0
    service_crashed: bool = False
    owner: str = "responsive"
    owner_fault_used: bool = False
    guardian: bool = True
    aliases: int = SUP|GUARD
    authorized: bool = True
    sent: bool = False
    inflight_go: bool = False
    pending_go: bool = False
    sealed: bool = False
    payload: bool = False
    stage1_starts: int = 0
    result_received: bool = False
    stopping: bool = False
    closure_observed: bool = False
    replacement: bool = False
    new_stage: int = -1
    new_consumed: bool = False
    new_running: bool = False
    bad_reauthorization: bool = False
    bad_generation_delivery: bool = False


@dataclass(frozen=True)
class Policy:
    name: str
    closure_observation_available: bool = True  # abstraction, not an OS proof
    actual_child_stop: bool = False
    infer_from_missing_receipt: bool = False
    infer_from_no_payload: bool = False
    reauthorize_same_stage: bool = False
    accept_old_go_in_new_generation: bool = False


def physically_closed(s):
    # A no-payload observation alone misses a launch gate that can still run.
    return not s.payload and not s.pending_go and not s.aliases and (
        s.owner=="dead" or s.sealed)


def steps(s,p):
    if s.service and not s.service_crashed:
        yield "service crashes; no automatic old GO replay",replace(s,service=False,service_crashed=True)
    if not s.service:
        yield "new service generation starts; original Child not restored",replace(s,service=True,epoch=1)
    if s.service and s.epoch==0 and s.authorized and not s.sent and not s.sealed:
        yield "original service sends previously authorized GO",replace(s,sent=True,inflight_go=True)
    if s.owner=="responsive" and s.inflight_go:
        if s.sealed:
            yield "old owner rejects sealed delayed GO",replace(s,inflight_go=False)
        elif not s.pending_go and not s.payload:
            yield "old owner consumes delayed GO even after service restart",replace(s,
                inflight_go=False,pending_go=True)
    if s.owner=="responsive" and s.pending_go and not s.sealed:
        yield "old pending GO spawns original stage 1",replace(s,
            pending_go=False,payload=True,stage1_starts=s.stage1_starts+1)
    if not s.owner_fault_used and s.owner=="responsive":
        yield "old owner freezes but keeps alias and pending gate",replace(s,
            owner="frozen",owner_fault_used=True)
        yield "old owner dies; its local pending gate dies, payload may survive",replace(s,
            owner="dead",owner_fault_used=True,pending_go=False,aliases=s.aliases&~SUP)
    if s.owner=="frozen":
        # Not a fair/automatic recovery action. Safety still must cover it.
        yield "outside harness resumes old owner",replace(s,owner="responsive")
        if p.actual_child_stop and s.service and s.epoch==0:
            yield "original service observes actual frozen Child stopped (ASSUMED)",replace(s,
                owner="dead",pending_go=False,aliases=s.aliases&~SUP)
    if not s.stopping:
        yield "deadline/owner-loss requests stop",replace(s,stopping=True)
    if s.stopping and s.owner=="responsive" and not s.sealed:
        yield "old owner seals launch gate before stop confirmation",replace(s,sealed=True,pending_go=False)
    if s.stopping and s.payload and (s.owner=="responsive" or s.guardian):
        yield "responsive scope owner observes old payload stopped",replace(s,payload=False)
    if s.payload:
        yield "old payload exits naturally (not assumed fair)",replace(s,payload=False)
    if s.stopping and s.guardian and not s.payload:
        # This may still leave a frozen supervisor alias and pending GO. It is
        # deliberately not treated as whole-scope closure.
        yield "guardian closes its alias after payload stop",replace(s,
            guardian=False,aliases=s.aliases&~GUARD)
    if s.owner=="responsive" and s.sealed and not s.payload:
        yield "responsive old owner closes alias and exits",replace(s,
            owner="dead",aliases=s.aliases&~SUP)
    if s.service and not s.closure_observed and p.closure_observation_available and physically_closed(s):
        yield "observe full old scope closed; result can remain missing",replace(s,closure_observed=True)
    if s.service and not s.replacement:
        allowed=s.closure_observed
        if p.infer_from_missing_receipt and not s.result_received:
            allowed=True
        if p.infer_from_no_payload and not s.payload:
            allowed=True
        if allowed:
            yield "authorize replacement generation",replace(s,replacement=True)
        else:
            yield "replacement request refuses absent closure evidence",s
    if s.service and s.replacement and s.new_stage<0:
        yield "authorize explicitly NEW stage 2",replace(s,new_stage=1)
        if p.reauthorize_same_stage:
            yield "BUG: reuse original stage 1 identity in replacement",replace(s,
                new_stage=0,bad_reauthorization=True)
    if s.replacement and s.inflight_go:
        if p.accept_old_go_in_new_generation:
            yield "BUG: replacement consumes old-generation GO",replace(s,
                inflight_go=False,new_stage=0,bad_generation_delivery=True)
        else:
            yield "replacement rejects delayed old-generation GO",replace(s,inflight_go=False)
    if s.new_stage>=0 and not s.new_consumed and not s.aliases:
        yield "replacement consumes own gate and spawns selected stage",replace(s,
            new_consumed=True,new_running=True,
            stage1_starts=s.stage1_starts+(s.new_stage==0))
    if s.new_running:
        yield "replacement command exits",replace(s,new_running=False)


def violations(s):
    bad=[]
    if s.closure_observed and not physically_closed(s):
        bad.append("ClosureEvidenceCoversPayloadGateAndAliases")
    if s.replacement and not s.closure_observed:
        bad.append("MissingReceiptOrNoPayloadIsNotClosure")
    if s.replacement and not physically_closed(s):
        bad.append("ReplacementRequiresOldScopeStopped")
    if s.bad_reauthorization:
        bad.append("ImmutableStageCannotBeReauthorized")
    if s.bad_generation_delivery:
        bad.append("OldGenerationGoCannotAuthorizeReplacement")
    if s.stage1_starts>1:
        bad.append("AtMostOneLaunchOfOriginalStage")
    if s.payload and s.new_running:
        bad.append("OldAndReplacementPayloadsDoNotOverlap")
    return bad


def explore(initial,p,limit=50000,predicate=None,skip_resumption=False):
    todo=deque([initial])
    pred={initial:None}
    while todo:
        s=todo.popleft()
        bad=violations(s)
        found=bool(bad) if predicate is None else predicate(s)
        if found:
            trace=[]
            cur=s
            while pred[cur] is not None:
                old,label=pred[cur]
                trace.append(dict(action=label,state=asdict(cur)))
                cur=old
            return dict(status="counterexample" if predicate is None else "witness found",
                        states=len(pred),violations=bad,trace=list(reversed(trace)))
        for label,nxt in steps(s,p):
            if skip_resumption and label.startswith("outside harness"):
                continue
            if nxt not in pred:
                pred[nxt]=(s,label)
                todo.append(nxt)
                if len(pred)>limit:
                    return dict(status="INCOMPLETE state limit",states=len(pred))
    return dict(status="bounded safety passed" if predicate is None else "unreachable in bounded graph",
                states=len(pred))


def main():
    base=Policy("base")
    initial=[
        ("bound, first GO never sent",State()),
        ("old GO inflight and owner frozen after service loss",
         State(service=False,service_crashed=True,owner="frozen",owner_fault_used=True,
               sent=True,inflight_go=True)),
        ("old GO consumed but not spawned; frozen owner after restart",
         State(epoch=1,service_crashed=True,owner="frozen",owner_fault_used=True,
               sent=True,pending_go=True)),
        ("old owner lost, payload already ran, result missing",
         State(epoch=1,service_crashed=True,owner="dead",owner_fault_used=True,
               aliases=GUARD,sent=True,payload=True,stage1_starts=1)),
    ]
    report={"safe":[],"negative_controls":[],"recovery_witnesses":[]}
    for name,s in initial:
        r=explore(s,base)
        report["safe"].append(dict(name=name,**r))
        print(name,r["status"],r["states"],flush=True)
        assert r["status"]=="bounded safety passed"
    negatives=[
        ("BUG missing receipt permits replacement",initial[1][1],replace(base,infer_from_missing_receipt=True)),
        ("BUG no payload permits replacement despite pending GO",initial[2][1],replace(base,infer_from_no_payload=True)),
        ("BUG original stage reauthorized after actual closure",initial[3][1],replace(base,reauthorize_same_stage=True)),
        ("BUG old GO accepted in new generation",initial[1][1],replace(base,accept_old_go_in_new_generation=True)),
    ]
    for name,s,p in negatives:
        r=explore(s,p)
        report["negative_controls"].append(dict(name=name,**r))
        print(name,r["status"],r["states"],flush=True)
        assert r["status"]=="counterexample"
    for name,s in initial[1:]:
        r=explore(s,base,predicate=lambda q:q.new_running and q.new_stage==1 and not q.result_received,
                  skip_resumption=True)
        report["recovery_witnesses"].append(dict(name=name,**r))
    duplicate=explore(initial[3][1],replace(base,reauthorize_same_stage=True),
                      predicate=lambda s:s.stage1_starts>1,skip_resumption=True)
    assert duplicate["status"]=="witness found"
    report["reauthorization_actual_duplicate_launch"]=duplicate
    Path("cross-boundary-results.json").write_text(json.dumps(report,indent=2)+"\n")


if __name__=="__main__":
    main()
