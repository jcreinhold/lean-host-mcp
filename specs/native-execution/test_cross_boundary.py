import unittest
from dataclasses import replace
from check_cross_boundary import State,Policy,SUP,GUARD,steps,physically_closed,explore


class CrossBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.policy=Policy("test")

    def test_frozen_pending_gate_is_not_closure_despite_no_payload(self):
        s=State(epoch=1,service_crashed=True,owner="frozen",owner_fault_used=True,
                sent=True,pending_go=True,stopping=True)
        guardian_closed=next(n for a,n in steps(s,self.policy) if a.startswith("guardian closes"))
        self.assertFalse(guardian_closed.payload)
        self.assertTrue(guardian_closed.pending_go)
        self.assertEqual(guardian_closed.aliases,SUP)
        self.assertFalse(physically_closed(guardian_closed))
        self.assertFalse(any(a=="authorize replacement generation" for a,_ in steps(guardian_closed,self.policy)))

    def test_free_lock_does_not_seal_a_live_launch_gate(self):
        s=State(owner="responsive",aliases=0,sent=True,pending_go=True)
        self.assertFalse(physically_closed(s))
        self.assertFalse(any(a=="authorize replacement generation" for a,_ in steps(s,self.policy)))

    def test_missing_result_does_not_block_after_actual_closure(self):
        s=State(epoch=1,service_crashed=True,owner="dead",owner_fault_used=True,
                guardian=False,aliases=0,sent=True,stage1_starts=1)
        observed=next(n for a,n in steps(s,self.policy) if a.startswith("observe full old scope closed"))
        replacement=next(n for a,n in steps(observed,self.policy) if a=="authorize replacement generation")
        self.assertTrue(replacement.replacement)
        self.assertFalse(replacement.result_received)
        self.assertEqual(replacement.stage1_starts,1)

    def test_no_old_closure_observation_is_not_replaced_by_missing_receipt(self):
        s=State(owner="dead",guardian=False,aliases=0)
        policy=replace(self.policy,closure_observation_available=False)
        self.assertFalse(any(a.startswith("observe full") or a=="authorize replacement generation"
                             for a,_ in steps(s,policy)))

    def test_already_sent_go_can_launch_after_service_restart(self):
        s=State(epoch=1,service_crashed=True,sent=True,inflight_go=True)
        consumed=next(n for a,n in steps(s,self.policy) if a.startswith("old owner consumes delayed GO"))
        spawned=next(n for a,n in steps(consumed,self.policy) if a.startswith("old pending GO spawns"))
        self.assertEqual(spawned.stage1_starts,1)

    def test_new_generation_rejects_old_go_even_after_resource_recovery(self):
        s=State(epoch=1,service_crashed=True,owner="dead",guardian=False,aliases=0,
                sent=True,inflight_go=True,closure_observed=True,replacement=True)
        rejected=next(n for a,n in steps(s,self.policy) if a.startswith("replacement rejects"))
        self.assertFalse(rejected.inflight_go)
        self.assertEqual(rejected.new_stage,-1)

    def test_no_automatic_resumption_witness_for_frozen_owner_after_service_loss(self):
        s=State(service=False,service_crashed=True,owner="frozen",owner_fault_used=True,
                sent=True,pending_go=True)
        result=explore(s,self.policy,predicate=lambda x:x.replacement,skip_resumption=True)
        self.assertEqual(result["status"],"unreachable in bounded graph")


if __name__=="__main__":
    unittest.main()
