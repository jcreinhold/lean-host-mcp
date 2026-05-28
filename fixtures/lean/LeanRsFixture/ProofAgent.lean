import LeanRsFixture.ProofSearchFacts

namespace LeanRsFixture.ProofAgent

open LeanRsFixture.ProofSearchFacts

theorem miniRatDenominatorStep (q : MiniRat) :
    cast (q.num * q.den) = q.num * q.den := by
  skip
  exact cast_num_den_helper q

/-- This docstring follows the theorem so overlay construction must not corrupt it. -/
theorem followingDocstring : True := by
  trivial

end LeanRsFixture.ProofAgent
