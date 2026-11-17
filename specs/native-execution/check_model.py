#!/usr/bin/env python3
"""Finite-state protocol model, not an OS simulator or implementation proof.

Run with the Python standard library only. Independent atomic transitions model
durable authorization, GO delivery, spawn and result publication. No transition
pretends SQLite and OS spawn are atomic. Locks model complete unions and alias
close semantics; these are refinement obligations, not tested OS properties.
"""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import dataclass, replace
import json
from pathlib import Path
import time


# Job phase: queued, supervisor has union, guardian registered, authorized,
# GO delivered, launched, closed. A real service may combine the first two.
QUEUED, ACQUIRED, READY, AUTH, GO, LAUNCHED, CLOSED = range(7)
NAMES = ("queued", "acquired", "ready", "authorized", "go", "launched", "closed")
SUP, GUARD = 1, 2
LEADER, DESCENDANT = 1, 2


@dataclass(frozen=True, slots=True)
class Job:
    phase: int = QUEUED
    owners: int = 0        # live custody aliases, independent of payload
    payload: int = 0       # live leader/descendant access, independent of status
    started: int = 0       # ground truth, not necessarily durably observed
    witnessed: bool = False
    stopping: bool = False
    result: str = "pending"
    generation: int = 0
    leader_exit: bool = False
    frozen: int = 0        # SIGSTOP/unresponsive owner is alive and retains aliases


@dataclass(frozen=True, slots=True)
class State:
    service: bool = True
    service_frozen: bool = False
    epoch: int = 0
    faults: int = 0
    jobs: tuple[Job, Job] = (Job(), Job())


@dataclass(frozen=True, slots=True)
class Policy:
    name: str
    domains: tuple[frozenset[str], frozenset[str]]
    modes: tuple[str, str] = ("write", "write")
    owners: int = SUP | GUARD
    max_faults: int = 1
    guardian_can_stop: bool = True
    # An external qualified containment authority can see/stop an ownerless scope.
    # This is an explicit platform assumption; the model does not establish it.
    external_containment: bool = False
    free_lock_recovery: bool = False
    release_on_leader_exit: bool = False
    replay_after_restart: bool = False
    allow_fork: bool = True
    allow_freeze: bool = False
    freeze_service: bool = False
    freeze_guardian: bool = False
    service_can_stop_frozen_owner: bool = False  # unqualified platform assumption
    external_can_stop_frozen_owner: bool = False  # stronger independent OS owner
    restart_enabled: bool = True

    def conflict(self, left: int, right: int) -> bool:
        return bool(self.domains[left] & self.domains[right]) and (
            self.modes[left] == "write" or self.modes[right] == "write"
        )


def updated(s: State, i: int, job: Job) -> State:
    jobs = list(s.jobs)
    jobs[i] = job
    return replace(s, jobs=tuple(jobs))


def steps(s: State, p: Policy, *, faults: bool = True):
    """Yield (label, action family, successor). Model loops include stuttering.

    Explicit client retries/GET/lost replies are stutters: neither allocates an
    attempt nor changes launch state. Durable IDs are fixed before this model
    starts. Separate API tests below check retry/retention cases.
    """
    yield "GET / lost reply / same-ID retry", "observe", s
    service_ready = s.service and not s.service_frozen
    if faults and s.service and s.faults < p.max_faults:
        yield "service crashes", "fault", replace(s, service=False, faults=s.faults + 1)
    if not s.service and p.restart_enabled:
        yield "service restarts with new generation", "restart", replace(
            s, service=True, service_frozen=False, epoch=s.epoch+1)
    if faults and p.freeze_service and s.service and not s.service_frozen and s.faults < p.max_faults:
        yield "service freezes; its process still exists", "fault", replace(
            s,service_frozen=True,faults=s.faults+1)
    if p.freeze_service and s.service and s.service_frozen:
        yield "outside harness resumes service", "external_resume_service", replace(s,service_frozen=False)

    for i, j in enumerate(s.jobs):
        prefix = f"job {i+1}: "

        def put(label: str, family: str, **fields):
            return prefix + label, family + str(i), updated(s, i, replace(j, **fields))

        if j.phase == CLOSED:
            continue
        if service_ready and j.phase == QUEUED:
            busy = any(
                k != i and p.conflict(i, k) and other.owners
                for k, other in enumerate(s.jobs)
            )
            # With a qualified platform census, ownerless live scopes still block.
            unsafe_scope = p.external_containment and any(
                k != i and p.conflict(i, k) and other.payload
                for k, other in enumerate(s.jobs)
            )
            earlier_conflicting = any(k < i and other.phase == QUEUED and p.conflict(i,k)
                                      for k,other in enumerate(s.jobs))
            if not busy and not unsafe_scope and not earlier_conflicting:
                yield put("acquire complete lock union", "admit", phase=ACQUIRED,
                          owners=SUP, generation=s.epoch)
            yield put("queue budget expires; never launched", "expire_queue",
                      phase=CLOSED, result="not_started")

        if j.phase == ACQUIRED and j.owners == SUP and not j.frozen & SUP and not j.stopping:
            label = ("guardian receives alias and acknowledges" if p.owners & GUARD
                     else "direct owner finishes scope preparation")
            yield put(label, "register",
                      owners=p.owners, phase=READY)
        if (j.phase == READY and j.owners & SUP and service_ready
                and j.generation == s.epoch and not j.stopping):
            yield put("commit launch authorization", "authorize", phase=AUTH)
        if j.phase == AUTH and j.owners & SUP and service_ready and not j.stopping:
            if j.generation == s.epoch:
                yield put("deliver one-use GO", "go", phase=GO)
        if j.phase == GO and j.owners & SUP and not j.frozen & SUP and not j.stopping:
            # GO already crossed the process boundary. The service can crash or
            # restart before the supervisor observes lifeline loss; local spawn
            # may still occur. Do not hide that window behind service liveness.
            yield put("spawn payload (not the same transaction)", "spawn",
                      phase=LAUNCHED, payload=LEADER, started=j.started + 1)
        if j.phase == LAUNCHED:
            if j.payload and j.owners & ~j.frozen and not j.witnessed and service_ready:
                yield put("durably observe payload spawn", "witness", witnessed=True)
            if p.allow_fork and j.payload == LEADER and not j.stopping and not j.leader_exit:
                yield put("payload forks a writing child", "fork", payload=LEADER | DESCENDANT)
            if j.payload & LEADER:
                yield put("leader exits (child may survive)", "leader_exit",
                          payload=j.payload & ~LEADER, leader_exit=True)
            if j.payload == DESCENDANT:
                yield put("last descendant exits", "descendant_exit", payload=0)
            if (j.payload == 0 and j.leader_exit and j.witnessed and service_ready
                    and j.owners & SUP and not j.frozen & SUP and not j.stopping):
                yield put("publish success after full-scope closure", "publish", result="success")

        if j.phase >= ACQUIRED and not j.stopping:
            # Eventual hard/idle deadline, explicit cancellation, or owner/lifeline
            # failure. Expiry is NOT lock release. Quiet output is not a predicate.
            yield put("deadline/cancel/owner-loss requests stop", "stop", stopping=True)
        if j.stopping and j.payload:
            responsive = j.owners & ~j.frozen
            can_stop = bool(responsive & SUP) or (bool(responsive & GUARD) and p.guardian_can_stop)
            if can_stop or p.external_containment:
                yield put("observe complete managed scope stopped", "cleanup", payload=0)
        if j.phase >= ACQUIRED and j.payload == 0:
            # No store operation is required to close descriptors. The service can
            # publish Unknown later if it lost the result observation.
            if j.result == "success" or j.stopping or not j.owners:
                result = j.result if j.result == "success" else (
                    "unknown" if j.phase >= AUTH else "not_started"
                )
                remaining = j.owners & j.frozen
                if remaining != j.owners or not j.owners:
                    yield put("close responsive aliases; frozen aliases remain" if remaining
                              else "close custody; preserve known/unknown result", "release",
                              owners=remaining, phase=j.phase if remaining else CLOSED, result=result)

        for owner,label in ((SUP,"supervisor"),(GUARD,"guardian")):
            original_parent_can_stop = (owner==SUP and p.service_can_stop_frozen_owner
                                       and service_ready and s.epoch == j.generation)
            if ((original_parent_can_stop or p.external_can_stop_frozen_owner)
                    and j.stopping and j.owners & j.frozen & owner):
                # NEW required identity-safe operations, not facts established
                # by this model or current production code. Observation of actual
                # owner exit, rather than a sent signal, closes the owner's alias.
                authority=("original service actual Child" if original_parent_can_stop else "independent OS owner")
                yield put(f"{authority} observes frozen {label} exit (ASSUMED)",
                          "kill_frozen_owner", owners=j.owners & ~owner, frozen=j.frozen & ~owner)

        if faults and s.faults < p.max_faults:
            for owner, label in ((SUP, "supervisor"), (GUARD, "guardian")):
                if j.owners & owner:
                    next_s = updated(s, i, replace(j, owners=j.owners & ~owner,
                                                  frozen=j.frozen & ~owner))
                    yield prefix + label + " crashes; only its alias closes", "fault", replace(
                        next_s, faults=s.faults + 1)
            if p.allow_freeze and j.owners & SUP and not j.frozen & SUP:
                frozen = updated(s,i,replace(j,frozen=j.frozen | SUP))
                yield prefix + "supervisor freezes; its alias remains", "fault", replace(
                    frozen,faults=s.faults+1)
            if p.freeze_guardian and j.owners & GUARD and not j.frozen & GUARD:
                frozen = updated(s,i,replace(j,frozen=j.frozen | GUARD))
                yield prefix + "guardian freezes; its alias remains", "fault", replace(
                    frozen,faults=s.faults+1)
        if p.allow_freeze and j.frozen & SUP:
            # An external resumption may happen but is NOT assumed fair. A
            # model requiring it would hide the failed self-healing property.
            yield put("outside harness resumes supervisor", "external_resume",
                      frozen=j.frozen & ~SUP)
        if p.freeze_guardian and j.frozen & GUARD:
            yield put("outside harness resumes guardian", "external_resume_guardian",
                      frozen=j.frozen & ~GUARD)

        # Deliberately broken designs, each used as a negative control.
        if p.free_lock_recovery and j.owners == 0 and j.phase >= ACQUIRED:
            yield put("BUG: infer closure from free lock only", "bad_recover",
                      phase=CLOSED, result="unknown")
        if p.release_on_leader_exit and j.leader_exit and j.payload:
            yield put("BUG: release custody at leader exit", "bad_release",
                      owners=0, phase=CLOSED, result="success")
        if p.replay_after_restart and service_ready and j.started and j.generation != s.epoch:
            if j.owners == p.owners and not j.stopping and j.started < 2:
                yield put("BUG: replay GO after service restart", "bad_replay",
                          generation=s.epoch, phase=GO)


def violations(s: State, p: Policy):
    if p.conflict(0, 1) and all(j.payload for j in s.jobs):
        yield "NoConflictingPayloadAccess"
    for i, j in enumerate(s.jobs):
        if j.started > 1:
            yield f"AtMostOneLaunch[{i+1}]"
        if j.result == "success" and (j.started != 1 or j.payload or not j.witnessed):
            yield f"NoFalseSuccess[{i+1}]"
        if j.phase == CLOSED and j.payload:
            yield f"ClosedMeansScopeStopped[{i+1}]"


def describe(s: State):
    return {"service": s.service, "service_frozen": s.service_frozen,
            "epoch": s.epoch, "faults": s.faults,
            "jobs": [dict(phase=NAMES[j.phase], owners=j.owners, payload=j.payload,
                          starts=j.started, witnessed=j.witnessed, stopping=j.stopping,
                          result=j.result, generation=j.generation, frozen=j.frozen) for j in s.jobs]}


def explore(p: Policy, *, limit: int, initial: State | None = None):
    begin = time.monotonic()
    initial = State() if initial is None else initial
    todo = deque([initial])
    pred = {initial: None}
    edges = 0
    overlap = False
    missing_custody = None
    while todo:
        s = todo.popleft()
        overlap |= all(j.payload for j in s.jobs)
        if missing_custody is None and any(j.payload and not j.owners for j in s.jobs):
            missing_custody = s
        bad = list(violations(s, p))
        if bad:
            trace = []
            cur = s
            while pred[cur] is not None:
                prev, label = pred[cur]
                trace.append({"action": label, "state": describe(cur)})
                cur = prev
            return {"name": p.name, "status": "counterexample", "states": len(pred),
                    "transitions": edges, "seconds": time.monotonic()-begin,
                    "violations": bad, "trace": list(reversed(trace))}
        for label, _, nxt in steps(s, p):
            edges += 1
            if nxt not in pred:
                pred[nxt] = (s, label)
                todo.append(nxt)
                if len(pred) > limit:
                    return {"name": p.name, "status": "INCOMPLETE state limit", "states": len(pred)}
    return {"name": p.name, "status": "bounded invariants passed", "states": len(pred),
            "transitions": edges, "seconds": time.monotonic()-begin,
            "concurrent_payloads_reachable": overlap,
            "ownerless_payload_reachable": missing_custody is not None}


def recovery_test(p: Policy):
    """A deterministic cleanup witness, NOT a fairness proof.

    Start with one hung payload; kill the supervisor. Do not rely on a natural
    payload exit. Run enabled recovery actions in a fixed round-robin priority.
    Report failure to make progress distinctly. Fairness hypotheses are stated
    in README and the TLA+ specification; these tests alone don't prove liveness.
    """
    active = Job(phase=LAUNCHED, owners=p.owners & ~SUP, payload=LEADER|DESCENDANT,
                 started=1, witnessed=False, generation=0)
    s = State(faults=1, jobs=(active, Job()))
    trace = []
    priorities = ("restart", "stop0", "cleanup0", "release0", "admit1", "register1",
                  "authorize1", "go1", "spawn1")
    for _ in range(20):
        bad = list(violations(s,p))
        if bad:
            return {"name":p.name, "recovery":"UNSAFE successor", "violations":bad,
                    "state":describe(s), "trace":trace}
        if s.jobs[1].payload:
            return {"name":p.name, "recovery":"successor started", "trace":trace}
        options = list(steps(s,p,faults=False))
        chosen = next((x for family in priorities for x in options if x[1] == family), None)
        if chosen is None:
            break
        label, _, nxt = chosen
        if nxt == s:
            break
        trace.append(label)
        s = nxt
    return {"name":p.name, "recovery":"stalled in witness", "state":describe(s), "trace":trace}


def strong_components(graph, nodes):
    """Iterative Kosaraju with real DFS finishing order."""
    done=set()
    order=[]
    for start in nodes:
        if start in done:
            continue
        done.add(start)
        stack=[(start,iter(graph[start]))]
        while stack:
            v,neighbors=stack[-1]
            try:
                w,_=next(neighbors)
            except StopIteration:
                order.append(v)
                stack.pop()
                continue
            if w in nodes and w not in done:
                done.add(w)
                stack.append((w,iter(graph[w])))
    reverse={v:[] for v in nodes}
    for v in nodes:
        for w,_ in graph[v]:
            if w in nodes:
                reverse[w].append(v)
    done=set()
    components=[]
    for start in reversed(order):
        if start in done:
            continue
        component=set((start,))
        done.add(start)
        stack=[start]
        while stack:
            v=stack.pop()
            for w in reverse[v]:
                if w not in done:
                    done.add(w)
                    component.add(w)
                    stack.append(w)
        components.append(component)
    return components


def check_termination(p: Policy, *, limit: int, initial: State | None = None, first_only: bool = False):
    """Find weakly-fair infinite executions avoiding all-jobs-closed.

    This is a bounded finite graph calculation, not a proof for arbitrary N.
    Required weak fairness: each job's deadline, stop observation, cleanup and
    custody release, frozen-owner termination/exit where available, plus service
    restart. Natural command exit is NOT assumed
    fair: a job may hang forever until killed. A full-scope cleanup transition
    assumes eventual identity-safe OS termination and observation.

    In an SCC, a weak-fairness action must either be disabled at some visited
    state or have a nonstuttering edge within the SCC. A finite walk can visit
    every SCC state and fairness edge repeatedly. All infinite walks have a
    recurrent SCC. If a maximal SCC cannot be fair because an always-enabled
    action has no internal edge, no subcycle can supply that missing edge.
    """
    initial=State() if initial is None else initial
    states=[initial]
    ids={initial:0}
    graph=[]
    enabled=[]
    todo=deque([initial])
    fair={"restart"}|{name+str(i) for i in range(2)
        for name in ("expire_queue","stop","cleanup","release","kill_frozen_owner")}
    while todo:
        s=todo.popleft()
        out=[]
        en=set()
        for _, family, nxt in steps(s,p):
            if nxt==s:
                continue
            if nxt not in ids:
                ids[nxt]=len(states)
                states.append(nxt)
                todo.append(nxt)
                if len(states)>limit:
                    return {"name":p.name,"status":"INCOMPLETE state limit"}
            out.append((ids[nxt],family))
            if family in fair:
                en.add(family)
        graph.append(out)
        enabled.append(en)
    candidates={i for i,s in enumerate(states) if not (
        s.jobs[0].phase==CLOSED if first_only else all(j.phase==CLOSED for j in s.jobs))}

    pending=strong_components(graph,candidates)
    examined=0
    while pending:
        component=pending.pop()
        examined+=1
        # Stuttering is always a legal step, even for a singleton SCC.
        common=set.intersection(*(enabled[v] for v in component))
        internal={a for v in component for w,a in graph[v] if w in component}
        violated=common-internal
        if not violated:
            v=next(iter(component))
            return {"name":p.name,"status":"fair nontermination counterexample",
                    "states":len(states),"recurrent_states":len(component),
                    "witness_state":describe(states[v]),"examined_scc":examined}
        # The violated action is enabled at EVERY node in this SCC and has NO
        # internal edge. No recurrent subcomponent can evade that weak fairness.
    return {"name":p.name,"status":"bounded eventual closure passed under stated fairness",
            "states":len(states),"examined_scc":examined}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, default=Path("results.json"))
    parser.add_argument("--limit", type=int, default=2_000_000)
    args = parser.parse_args()
    both = (frozenset(("A","B")), frozenset(("B",)))
    separate = (frozenset(("A",)), frozenset(("B",)))
    configs = [
        Policy("dual single-failure overlapping writers", both),
        Policy("dual single-failure separate writers", separate),
        Policy("dual single-failure overlapping readers", both, modes=("read","read")),
        Policy("dual two-failure no containment", both, max_faults=2),
        Policy("dual two-failure qualified containment ASSUMED", both, max_faults=2,
               external_containment=True),
        Policy("direct owner one-failure no containment", both, owners=SUP),
        Policy("direct owner qualified containment ASSUMED", both, owners=SUP,
               external_containment=True),
        Policy("BUG free-lock recovery", both, max_faults=2, free_lock_recovery=True),
        Policy("BUG leader-exit release", both, release_on_leader_exit=True),
        Policy("BUG GO replay", both, replay_after_restart=True),
    ]
    results=[]
    for p in configs:
        r=explore(p, limit=args.limit)
        results.append(r)
        print(p.name, r["status"], r["states"], flush=True)
    recoveries=[recovery_test(p) for p in (configs[0], replace(configs[0],
                 name="guardian cannot safely stop after parent death", guardian_can_stop=False),
                 configs[5], configs[6])]
    liveness=[check_termination(p,limit=args.limit) for p in (
        configs[0],configs[4],configs[5],configs[6],
        replace(configs[0], name="guardian cannot stop hung scope",guardian_can_stop=False))]
    output={"safety":results,"recovery_witnesses":recoveries,"liveness":liveness}
    args.output.write_text(json.dumps(output,indent=2)+"\n")
    expected=["bounded invariants passed"]*3+["counterexample","bounded invariants passed",
        "counterexample","bounded invariants passed"]+["counterexample"]*3
    if [r["status"] for r in results] != expected:
        raise SystemExit("Unexpected model result: inspect results; do not call it passed")
    if [r["status"] for r in liveness] != [
        "bounded eventual closure passed under stated fairness",
        "bounded eventual closure passed under stated fairness",
        "fair nontermination counterexample",
        "bounded eventual closure passed under stated fairness",
        "fair nontermination counterexample",
    ]:
        raise SystemExit("Unexpected liveness result")


if __name__=="__main__":
    main()
