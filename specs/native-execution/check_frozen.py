#!/usr/bin/env python3
"""Focused model refinement from the SIGSTOP custody experiment.

No process signals or native experiments run here. A model assertion about an
identity-safe kill remains an assumption until the separate Rust fixture tests it.
"""
from collections import deque
from dataclasses import replace
import json
from pathlib import Path
from check_model import (State, Job, Policy, SUP, GUARD, LEADER, DESCENDANT,
                         LAUNCHED, CLOSED, steps, describe, explore, check_termination)


def find_stopped_payload_held_alias(initial,policy):
    """Shortest trace without relying on external SIGCONT/harness help."""
    todo=deque([initial])
    previous={initial:None}
    while todo:
        s=todo.popleft()
        j=s.jobs[0]
        if j.payload==0 and j.owners==SUP and j.frozen==SUP and j.phase!=CLOSED:
            trace=[]
            while previous[s] is not None:
                old,label=previous[s]
                trace.append({"action":label,"state":describe(s)})
                s=old
            return list(reversed(trace))
        for label,family,nxt in steps(s,policy):
            if family=="external_resume0":
                continue
            if nxt not in previous:
                previous[nxt]=(s,label)
                todo.append(nxt)
    return None


def main():
    domains=(frozenset("AB"),frozenset("B"))
    job=Job(phase=LAUNCHED,owners=SUP|GUARD,frozen=SUP,
            payload=LEADER|DESCENDANT,started=1,generation=0)
    alive=State(faults=1,jobs=(job,Job()))
    dead=replace(alive,service=False,faults=2)
    base=Policy("frozen supervisor, guardian alive; no owner-stop authority",domains,
                allow_freeze=True,max_faults=1)
    cases=[
        (base,alive),
        (replace(base,name="original service holds actual supervisor Child; safe stop ASSUMED",
                 service_can_stop_frozen_owner=True),alive),
        (replace(base,name="service died; restart cannot recreate actual Child authority",
                 max_faults=2,service_can_stop_frozen_owner=True),dead),
        (replace(base,name="service died; independent OS owner-stop authority ASSUMED",
                 max_faults=2,external_can_stop_frozen_owner=True),dead),
    ]
    report={"source":"/private/tmp/lean-custody-experiment-20261117/README.md",
            "scope":"first job resource closure; successor queue status does not define recovery",
            "cases":[],"payload_stopped_lock_retained_trace":find_stopped_payload_held_alias(alive,base)}
    for policy,initial in cases:
        safety=explore(policy,limit=500000,initial=initial)
        closure=check_termination(policy,limit=500000,initial=initial,first_only=True)
        report["cases"].append({"name":policy.name,"initial":describe(initial),
                                "safety":safety,"first_scope_closure":closure})
        print(policy.name, safety["status"], closure["status"],flush=True)
    Path("frozen-results.json").write_text(json.dumps(report,indent=2)+"\n")
    assert report["payload_stopped_lock_retained_trace"]
    assert all(c["safety"]["status"]=="bounded invariants passed" for c in report["cases"])
    assert [c["first_scope_closure"]["status"] for c in report["cases"]]==[
        "fair nontermination counterexample",
        "bounded eventual closure passed under stated fairness",
        "fair nontermination counterexample",
        "bounded eventual closure passed under stated fairness",
    ]


if __name__=="__main__":
    main()
