"""Model-checker regressions and small API/deadline abstractions.

These tests do not use SQLite, launch OS processes, or validate production code.
They check the intended abstract contract and the checker itself.
"""
import unittest
from dataclasses import dataclass, replace
from check_model import (
    Job, State, Policy, SUP, GUARD, LEADER, AUTH, GO, LAUNCHED, CLOSED,
    READY, steps, violations, strong_components,
)


class ModelTests(unittest.TestCase):
    def setUp(self):
        self.p=Policy("test", (frozenset("AB"),frozenset("B")))

    def test_spawn_may_follow_service_crash_after_go(self):
        j=Job(phase=GO,owners=SUP|GUARD)
        s=State(service=False,faults=1,jobs=(j,Job()))
        self.assertTrue(any(a=="spawn0" for _,a,_ in steps(s,self.p)))

    def test_spawn_may_precede_guardian_death_observation(self):
        j=Job(phase=GO,owners=SUP)
        s=State(faults=1,jobs=(j,Job()))
        self.assertTrue(any(a=="spawn0" for _,a,_ in steps(s,self.p)))

    def test_authorized_without_spawn_witness_reports_unknown(self):
        j=Job(phase=AUTH,owners=SUP|GUARD,stopping=True)
        nxt=next(n for _,a,n in steps(State(jobs=(j,Job())),self.p) if a=="release0")
        self.assertEqual(nxt.jobs[0].result,"unknown")

    def test_preauthorization_can_report_not_started(self):
        j=Job(phase=READY,owners=SUP|GUARD,stopping=True)
        nxt=next(n for _,a,n in steps(State(jobs=(j,Job())),self.p) if a=="release0")
        self.assertEqual(nxt.jobs[0].result,"not_started")

    def test_success_and_resource_release_remain_distinct(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,started=1,witnessed=True,
              leader_exit=True,result="success")
        s=State(jobs=(j,Job()))
        self.assertFalse(any(a=="admit1" for _,a,_ in steps(s,self.p)))
        self.assertEqual(list(violations(s,self.p)),[])

    def test_unknown_result_does_not_block_new_work_after_closure(self):
        j=Job(phase=CLOSED,started=1,result="unknown")
        s=State(jobs=(j,Job()))
        self.assertTrue(any(a=="admit1" for _,a,_ in steps(s,self.p)))

    def test_restart_generation_is_fresh_each_time(self):
        s=State(service=False,epoch=1,faults=2)
        nxt=next(n for _,a,n in steps(s,self.p) if a=="restart")
        self.assertEqual(nxt.epoch,2)

    def test_single_leader_exit_does_not_close_writing_descendant(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,payload=2,started=1,
              leader_exit=True,witnessed=True)
        s=State(jobs=(j,Job()))
        self.assertFalse(any(a in ("publish0","release0") for _,a,_ in steps(s,self.p)))

    def test_stopped_payload_does_not_free_frozen_supervisor_alias(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,frozen=SUP,started=1,stopping=True)
        nxt=next(n for _,a,n in steps(State(jobs=(j,Job())),self.p) if a=="release0")
        self.assertEqual(nxt.jobs[0].owners,SUP)
        self.assertEqual(nxt.jobs[0].frozen,SUP)
        self.assertNotEqual(nxt.jobs[0].phase,CLOSED)

    def test_original_service_may_stop_actual_frozen_child_only_under_assumption(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,frozen=SUP,started=1,stopping=True)
        s=State(jobs=(j,Job()))
        self.assertFalse(any(a=="kill_frozen_owner0" for _,a,_ in steps(s,self.p)))
        policy=replace(self.p,service_can_stop_frozen_owner=True)
        self.assertTrue(any(a=="kill_frozen_owner0" for _,a,_ in steps(s,policy)))

    def test_restart_does_not_restore_parent_wait_authority(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,frozen=SUP,started=1,stopping=True,generation=0)
        s=State(epoch=1,jobs=(j,Job()))
        policy=replace(self.p,service_can_stop_frozen_owner=True)
        self.assertFalse(any(a=="kill_frozen_owner0" for _,a,_ in steps(s,policy)))

    def test_frozen_supervisor_cannot_consume_go_or_clean_payload(self):
        j=Job(phase=GO,owners=SUP,frozen=SUP)
        s=State(jobs=(j,Job()))
        self.assertFalse(any(a=="spawn0" for _,a,_ in steps(s,self.p)))
        j=replace(j,phase=LAUNCHED,started=1,payload=LEADER,stopping=True)
        s=State(jobs=(j,Job()))
        self.assertFalse(any(a=="cleanup0" for _,a,_ in steps(s,self.p)))

    def test_frozen_service_does_not_authorize_new_work(self):
        j=Job(phase=READY,owners=SUP|GUARD)
        s=State(service_frozen=True,jobs=(j,Job()))
        self.assertFalse(any(a=="authorize0" for _,a,_ in steps(s,self.p)))
        j=replace(j,phase=GO)
        self.assertTrue(any(a=="spawn0" for _,a,_ in steps(replace(s,jobs=(j,Job())),self.p)))

    def test_frozen_guardian_alias_survives_payload_cleanup(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,frozen=GUARD,started=1,stopping=True)
        s=State(jobs=(j,Job()))
        nxt=next(n for _,a,n in steps(s,self.p) if a=="release0")
        self.assertEqual(nxt.jobs[0].owners,GUARD)
        self.assertNotEqual(nxt.jobs[0].phase,CLOSED)
        policy=replace(self.p,service_can_stop_frozen_owner=True)
        self.assertFalse(any(a=="kill_frozen_owner0" for _,a,_ in steps(nxt,policy)))

    def test_frozen_guardian_alone_cannot_execute_cleanup(self):
        j=Job(phase=LAUNCHED,owners=GUARD,frozen=GUARD,payload=LEADER,started=1,stopping=True)
        self.assertFalse(any(a=="cleanup0" for _,a,_ in steps(State(jobs=(j,Job())),self.p)))

    def test_responsive_supervisor_does_not_need_service_to_stop_payload(self):
        j=Job(phase=LAUNCHED,owners=SUP|GUARD,payload=LEADER,started=1,stopping=True)
        s=State(service=False,jobs=(j,Job()))
        self.assertTrue(any(a=="cleanup0" for _,a,_ in steps(s,self.p)))

    def test_scc_three_node_regression(self):
        graph=[[(1,"a"),(2,"a")],[],[(1,"a")]]
        self.assertEqual({frozenset(x) for x in strong_components(graph,{0,1,2})},
                         {frozenset((i,)) for i in range(3)})

    def test_scc_every_three_node_directed_graph(self):
        # Independent, slow mutual-reachability definition is cheap at n=3.
        for mask in range(1<<9):
            graph=[[(j,"a") for j in range(3) if mask & (1<<(3*i+j))] for i in range(3)]
            reaches=[]
            for start in range(3):
                found={start}
                todo=[start]
                while todo:
                    for nxt,_ in graph[todo.pop()]:
                        if nxt not in found:
                            found.add(nxt)
                            todo.append(nxt)
                reaches.append(found)
            expected={frozenset(j for j in range(3) if j in reaches[i] and i in reaches[j])
                      for i in range(3)}
            self.assertEqual({frozenset(c) for c in strong_components(graph,{0,1,2})},expected)


class IdentityStore:
    """Tiny in-memory abstract contract, NOT a proposed database implementation."""
    def __init__(self):
        self.epoch=0
        self.rows={}

    def create(self,key,intent):
        if key in self.rows:
            old,status=self.rows[key]
            if old!=intent:
                return "intent_mismatch"
            return status
        if key[0]!=self.epoch:
            return "expired"
        self.rows[key]=(intent,"queued")
        return "queued"

    def get(self,key):
        return self.rows.get(key,(None,"expired" if key[0]<self.epoch else "absent"))[1]

    def expire_result(self,key):
        intent,_=self.rows[key]
        self.rows[key]=(intent,"expired")

    def rotate_and_prune(self):
        self.epoch+=1
        # Creation epochs bound terminal tombstone retention; they must never
        # erase accepted live work. Resource-scope retention is modeled elsewhere.
        self.rows={k:v for k,v in self.rows.items()
                   if k[0]>=self.epoch or v[1]!="expired"}


class IdentityTests(unittest.TestCase):
    def test_lost_create_reply_and_retry_keep_same_attempt(self):
        db=IdentityStore()
        key=(0,"client-persisted-nonce")
        db.create(key,"command-A")  # Lose the reply, not the durable row.
        self.assertEqual(db.create(key,"command-A"),"queued")
        self.assertEqual(len(db.rows),1)

    def test_changed_intent_same_id_never_allocates_new_attempt(self):
        db=IdentityStore()
        db.create((0,"nonce"),"command-A")
        self.assertEqual(db.create((0,"nonce"),"command-B"),"intent_mismatch")
        self.assertEqual(len(db.rows),1)

    def test_expired_open_epoch_keeps_dedup_tombstone(self):
        db=IdentityStore()
        key=(0,"nonce")
        db.create(key,"command-A")
        db.expire_result(key)
        self.assertEqual(db.create(key,"command-A"),"expired")

    def test_closed_epoch_prevents_resurrection_after_prune(self):
        db=IdentityStore()
        key=(0,"nonce")
        db.create(key,"command-A")
        db.expire_result(key)
        db.rotate_and_prune()
        self.assertEqual(db.get(key),"expired")
        self.assertEqual(db.create(key,"command-A"),"expired")
        self.assertEqual(db.rows,{})

    def test_epoch_rotation_retains_accepted_queued_work(self):
        db=IdentityStore()
        key=(0,"live")
        db.create(key,"command-A")
        db.rotate_and_prune()
        self.assertEqual(db.get(key),"queued")
        self.assertEqual(db.create(key,"command-A"),"queued")

    def test_get_unknown_id_does_not_create_work(self):
        db=IdentityStore()
        self.assertEqual(db.get((0,"unknown")),"absent")
        self.assertEqual(db.rows,{})

    def test_new_stage_requires_new_explicit_id(self):
        db=IdentityStore()
        db.create((0,"stage-1"),"command-A")
        db.expire_result((0,"stage-1"))
        self.assertEqual(db.create((0,"stage-1"),"command-A"),"expired")
        self.assertEqual(db.create((0,"stage-2"),"command-B"),"queued")


@dataclass(frozen=True)
class Deadline:
    hard: int = 6
    soft: int | None = 3
    stopping: bool = False

    def event(self,kind,now):
        expired=now>=self.hard or (self.soft is not None and now>=self.soft)
        if self.stopping or expired:
            return replace(self,stopping=True)
        if kind=="activity":
            return replace(self,soft=min(now+3,self.hard))
        if kind=="measurement_unavailable":
            return replace(self,soft=None)  # Fall back to unchanged hard deadline.
        return self  # GET, heartbeat, quiet output do not count as useful work.


class DeadlineTests(unittest.TestCase):
    def test_all_event_sequences_up_to_hard_limit(self):
        # Complete exploration of this small deadline automaton, including
        # observation loss and renew/expiry ordering at the same timestamp.
        states={Deadline()}
        for now in range(8):
            nxt=set()
            for d in states:
                for kind in ("activity","heartbeat","get","quiet","measurement_unavailable"):
                    r=d.event(kind,now)
                    self.assertEqual(r.hard,6)
                    self.assertFalse(d.stopping and not r.stopping)
                    if now>=6:
                        self.assertTrue(r.stopping)
                    if kind in ("heartbeat","get","quiet") and not r.stopping:
                        self.assertEqual(r.soft,d.soft)
                    nxt.add(r)
            states=nxt

    def test_activity_does_not_revive_after_expiry(self):
        d=Deadline()
        self.assertTrue(d.event("activity",3).stopping)

    def test_missing_measurement_is_not_zero_activity(self):
        d=Deadline().event("measurement_unavailable",1)
        self.assertIsNone(d.soft)
        self.assertFalse(d.event("quiet",5).stopping)
        self.assertTrue(d.event("quiet",6).stopping)


if __name__=="__main__":
    unittest.main()
