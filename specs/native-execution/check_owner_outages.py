#!/usr/bin/env python3
"""Focused frozen service/guardian and restart cases. No native processes."""
from dataclasses import replace
from pathlib import Path
import json
from check_model import (State,Job,Policy,SUP,GUARD,LEADER,DESCENDANT,LAUNCHED,
                         describe,explore,check_termination)


def main():
    domains=(frozenset("AB"),frozenset("B"))
    job=Job(phase=LAUNCHED,owners=SUP|GUARD,payload=LEADER|DESCENDANT,
            started=1,generation=0)
    base=Policy("base",domains,allow_freeze=True,freeze_service=True,
                freeze_guardian=True,max_faults=1)
    cases=[
        (replace(base,name="service frozen; both scope owners responsive"),
         State(service_frozen=True,faults=1,jobs=(job,Job())),True),
        (replace(base,name="guardian frozen; supervisor responsive; no guardian-stop authority"),
         State(faults=1,jobs=(replace(job,frozen=GUARD),Job())),False),
        (replace(base,name="guardian frozen; original service can stop supervisor only",
                 service_can_stop_frozen_owner=True),
         State(faults=1,jobs=(replace(job,frozen=GUARD),Job())),False),
        (replace(base,name="guardian frozen; independent guardian-stop authority ASSUMED",
                 external_can_stop_frozen_owner=True),
         State(faults=1,jobs=(replace(job,frozen=GUARD),Job())),True),
        (replace(base,name="service and supervisor frozen; service Child operation unavailable",
                 service_can_stop_frozen_owner=True,max_faults=2),
         State(service_frozen=True,faults=2,jobs=(replace(job,frozen=SUP),Job())),False),
        (replace(base,name="service restarts; original supervisor responsive and still holds alias",
                 max_faults=1),
         State(service=False,faults=1,jobs=(job,Job())),True),
        (replace(base,name="service restarts; original supervisor frozen and still holds alias",
                 service_can_stop_frozen_owner=True,max_faults=2),
         State(service=False,faults=2,jobs=(replace(job,frozen=SUP),Job())),False),
        (replace(base,name="supervisor gone; guardian frozen; no responsive custody owner",max_faults=2),
         State(faults=2,jobs=(replace(job,owners=GUARD,frozen=GUARD),Job())),False),
    ]
    report=[]
    for policy,initial,expected in cases:
        safety=explore(policy,limit=500000,initial=initial)
        closure=check_termination(policy,limit=500000,initial=initial,first_only=True)
        row=dict(name=policy.name,initial=describe(initial),safety=safety,first_scope_closure=closure)
        report.append(row)
        print(policy.name,safety["status"],closure["status"],closure.get("states"),flush=True)
        assert safety["status"]=="bounded invariants passed"
        assert closure["status"]==("bounded eventual closure passed under stated fairness" if expected
                                    else "fair nontermination counterexample")
    # A frozen service cannot accept the second job. Resource self-cleanup is
    # not proof that the service itself restarts. No external SIGCONT is fair.
    policy,initial,_=cases[0]
    report.append(dict(name="frozen service: all-jobs completion is a DIFFERENT property",
        all_jobs_closure=check_termination(policy,limit=500000,initial=initial)))
    assert report[-1]["all_jobs_closure"]["status"]=="fair nontermination counterexample"
    Path("owner-outage-results.json").write_text(json.dumps(report,indent=2)+"\n")


if __name__=="__main__":
    main()
