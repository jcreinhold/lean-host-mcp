import unittest
from check_late_controls import KINDS,explore

class LateControlTests(unittest.TestCase):
    def test_all_thirty_fixed_two_delivery_slices(self):
        pairs=[(k,k) for k in KINDS]+[(KINDS[i],KINDS[j])
              for i in range(4) for j in range(i+1,4)]
        for cut in ("second pending","second running","generation replaced"):
            for kinds in pairs:
                with self.subTest(cut=cut,kinds=kinds):
                    result=explore(cut,kinds)
                    self.assertEqual(result["status"],"bounded safety passed")
                    self.assertIn("both actual deliveries received",result["coverage"])
                    if cut!="generation replaced":
                        self.assertIn("old control delivered while second stage pending/running",
                                      result["coverage"])
                    self.assertIn("old control delivered after generation replacement",
                                  result["coverage"])
                    self.assertIn("old control delivered after service restart",result["coverage"])
                    if "cleanup" in kinds:
                        self.assertIn("cleanup callback actually received",result["coverage"])

    def test_cancel_wrong_stage_negative(self):
        self.assertEqual(explore("second running",("cancel","cancel"),"cancel current")
                         ["status"],"counterexample")

    def test_result_declares_idle_negative(self):
        self.assertEqual(explore("second running",("result","result"),"result idle")
                         ["status"],"counterexample")

    def test_ack_wrong_result_negative(self):
        result=explore("second running",("ack","ack"),"ack current")
        self.assertIn("OldAckMustNotDeleteSecondStageResult",result["violations"])

    def test_cleanup_wrong_stage_negative(self):
        result=explore("second running",("cleanup","cleanup"),"callback current")
        self.assertIn("OldCallbackCannotChangeCurrentStageOrGeneration",result["violations"])

    def test_cleanup_wrong_generation_negative(self):
        result=explore("generation replaced",("cleanup","cleanup"),"callback generation")
        self.assertIn("OldCallbackCannotChangeCurrentStageOrGeneration",result["violations"])

if __name__=="__main__":
    unittest.main()
