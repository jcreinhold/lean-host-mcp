namespace LeanRsFixture.ProofActions

theorem closedTheorem : True := by
  trivial

theorem stepTheorem : True := by
  skip
  trivial

theorem sorryTheorem : True := by
  sorry

-- A single-`by` lemma whose first tactic introduces binders. A from-scratch
-- tactic block belongs at the pristine entry goal (the default); spliced after
-- `intro p hp` it would re-introduce binders already in scope and fail.
theorem entryBinderTheorem : ∀ (p : Prop), p → p := by
  intro p hp
  exact hp

end LeanRsFixture.ProofActions
