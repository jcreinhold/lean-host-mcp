import unittest
from dataclasses import replace
from check_stages import State,Frame,Policy,steps,find_trace


class StageTests(unittest.TestCase):
    def setUp(self):
        self.p=Policy("test")
        self.key=(0,0,0)
        self.go=Frame("go",0,0,0,0,0)
        self.s=State(created=3,ready=3,scope_owner=0,bindings=((0,0),None),
                     issued=frozenset((self.key,)),sent=frozenset((self.key,)),
                     authorization_epochs=(0,-1),idle=False)

    def test_duplicate_go_after_consume_before_spawn_is_rejected(self):
        s=replace(self.s,consumed=frozenset((self.key,)),pending=(self.go,None),messages=(self.go,))
        r=next(n for a,n in steps(s,self.p) if "rejects duplicate/busy GO" in a)
        self.assertEqual(r.pending,(self.go,None))
        self.assertEqual(r.starts,(0,0))

    def test_duplicate_supervisor_cannot_consume_another_owners_go(self):
        s=replace(self.s,messages=(replace(self.go,destination=1),))
        r=next(n for a,n in steps(s,self.p) if "rejects mismatched envelope" in a)
        self.assertEqual(r.pending,(None,None))

    def test_service_death_does_not_cancel_a_local_pending_spawn_by_magic(self):
        s=replace(self.s,service=False,crashed=True,pending=(self.go,None),
                  consumed=frozenset((self.key,)))
        self.assertTrue(any("spawns stage 1" in a for a,_ in steps(s,self.p)))

    def test_binding_and_initial_go_are_separate_events(self):
        s=State(created=1,ready=1,scope_owner=0)
        bound=next(n for a,n in steps(s,self.p) if "START(1) commits binding/authorization only" in a)
        self.assertEqual(bound.bindings[0],(0,0))
        self.assertEqual(bound.authorization_epochs[0],0)
        self.assertEqual(bound.sent,frozenset())
        self.assertFalse(any(f.kind=="go" for f in bound.messages))
        sent=next(n for a,n in steps(bound,self.p) if "enqueue first GO(1)" in a)
        self.assertEqual(sent.sent,frozenset((self.key,)))

    def test_crash_after_binding_before_send_does_not_restart_dispatch(self):
        bound=replace(self.s,sent=frozenset(),messages=())
        down=next(n for a,n in steps(bound,self.p) if a.startswith("service crashes"))
        restarted=next(n for a,n in steps(down,self.p) if a.startswith("service restarts"))
        self.assertEqual(restarted.authorization_epochs[0],0)
        self.assertEqual(restarted.epoch,1)
        self.assertFalse(any("enqueue first GO(1)" in a for a,_ in steps(restarted,self.p)))

    def test_network_cannot_redeliver_a_go_that_was_never_sent(self):
        bound=replace(self.s,sent=frozenset(),messages=())
        self.assertFalse(any(a.startswith("network duplicates/delays GO(1)") for a,_ in steps(bound,self.p)))

    def test_sent_old_go_can_arrive_after_restart_before_failure_detection(self):
        old=replace(self.s,service=True,epoch=1,crashed=True,messages=(self.go,))
        received=next(n for a,n in steps(old,self.p) if "consumes one-use GO(1)" in a)
        self.assertTrue(any("spawns stage 1" in a for a,_ in steps(received,self.p)))

    def test_cancel_before_go_seals_gate_and_prevents_late_launch(self):
        s=replace(self.s,messages=(replace(self.go,kind="cancel"),))
        sealed=next(n for a,n in steps(s,self.p) if "seals never-consumed gate before GO" in a)
        self.assertIn(self.key,sealed.sealed)
        delayed=replace(sealed,messages=(self.go,))
        self.assertTrue(any("rejects duplicate/busy GO(1)" in a for a,_ in steps(delayed,self.p)))

    def test_spawn_failure_seals_gate_and_late_go_does_not_retry(self):
        s=replace(self.s,pending=(self.go,None),consumed=frozenset((self.key,)))
        r=next(n for a,n in steps(s,self.p) if "observes spawn failure" in a)
        self.assertIn(self.key,r.consumed)
        self.assertIn(self.key,r.sealed)
        r=replace(r,messages=(self.go,))
        self.assertTrue(any("rejects duplicate/busy" in a for a,_ in steps(r,self.p)))

    def active_stage2(self,kind):
        second=Frame("go",1,0,0,0,1)
        return replace(self.s,bindings=((0,0),(0,0)),
            issued=frozenset(((0,0,0),(1,0,0))),consumed=frozenset(((0,0,0),(1,0,0))),
            sent=frozenset(((0,0,0),(1,0,0))),authorization_epochs=(0,0),
            sealed=frozenset(((0,0,0),)),finished=1,terminal=1,
            live=(second,None),messages=(replace(self.go,kind=kind),))

    def test_old_cancel_cannot_cancel_active_stage2(self):
        s=self.active_stage2("cancel")
        r=next(n for a,n in steps(s,self.p) if "old cancel" in a)
        self.assertEqual(r.live,s.live)
        self.assertFalse(r.cross_cancel)

    def test_old_result_cannot_mark_active_stage2_idle(self):
        s=self.active_stage2("result")
        r=next(n for a,n in steps(s,self.p) if "receive old result" in a)
        self.assertEqual(r.live,s.live)
        self.assertFalse(r.idle)

    def test_old_ack_cannot_remove_consumed_gate_or_active_stage2(self):
        s=self.active_stage2("ack")
        r=next(n for a,n in steps(s,self.p) if "receive old ACK" in a)
        self.assertEqual(r.live,s.live)
        self.assertEqual(r.consumed,s.consumed)

    def test_old_scope_go_cannot_enter_replacement_generation(self):
        s=replace(self.s,generations=(1,0),messages=(self.go,))
        r=next(n for a,n in steps(s,self.p) if "rejects mismatched envelope" in a)
        self.assertEqual(r.pending,(None,None))

    def test_altered_intent_is_rejected(self):
        s=replace(self.s,messages=(replace(self.go,intent=1),))
        self.assertTrue(any("rejects mismatched envelope" in a for a,_ in steps(s,self.p)))

    def test_changed_start_intent_reaches_receiver_and_is_rejected(self):
        frame=Frame("start",0,-1,-1,-1,1)
        s=replace(self.s,messages=(frame,))
        r=next(n for a,n in steps(s,self.p) if "receiver rejects changed immutable intent" in a)
        self.assertEqual(r.bindings,self.s.bindings)
        self.assertEqual(r.pending,(None,None))

    def test_identical_start_after_result_expiry_returns_old_binding(self):
        frame=Frame("start",0,-1,-1,-1,0)
        s=replace(self.s,messages=(frame,),terminal=1,retained=0)
        r=next(n for a,n in steps(s,self.p) if "receiver returns same binding/result" in a)
        self.assertEqual(r.bindings,self.s.bindings)
        self.assertEqual(r.messages,())
        self.assertEqual(r.pending,(None,None))

    def test_cancel_request_is_not_process_exit(self):
        frame=replace(self.go,kind="cancel")
        s=replace(self.s,live=(self.go,None),messages=(frame,))
        r=next(n for a,n in steps(s,self.p) if "records cancel" in a)
        self.assertEqual(r.live,s.live)
        self.assertEqual(r.finished,0)
        self.assertEqual(r.terminal,0)
        self.assertFalse(any("START(2) commits" in a for a,_ in steps(r,self.p)))
        closed=next(n for a,n in steps(r,self.p) if "observes cancelled stage" in a)
        self.assertEqual(closed.live,(None,None))
        self.assertEqual(closed.finished,1)

    def test_spawn_may_race_a_cancel_request_until_actual_stop_observation(self):
        s=replace(self.s,pending=(self.go,None),consumed=frozenset((self.key,)),
                  messages=(replace(self.go,kind="cancel"),))
        requested=next(n for a,n in steps(s,self.p) if "records cancel" in a)
        self.assertTrue(any("spawns stage 1" in a for a,_ in steps(requested,self.p)))
        stopped=next(n for a,n in steps(requested,self.p) if "observes cancelled stage" in a)
        self.assertFalse(any("spawns stage 1" in a for a,_ in steps(stopped,self.p)))

    def test_broken_rebind_can_really_launch_same_stage_twice(self):
        result=find_trace(replace(self.p,rebind_after_restart=True),lambda s:max(s.starts)>1)
        self.assertEqual(result["status"],"witness found")
        self.assertGreater(max(result["trace"][-1]["state"]["starts"]),1)


if __name__=="__main__":
    unittest.main()
