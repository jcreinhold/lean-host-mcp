namespace LeanRsFixture.ProofSearchFacts

structure MiniRat where
  num : Nat
  den : Nat

def cast (n : Nat) : Nat := n

theorem cast_num_den_helper (q : MiniRat) : cast (q.num * q.den) = q.num * q.den := by
  rfl

theorem den_mul_num_comm (q : MiniRat) : q.den * q.num = q.num * q.den := by
  exact Nat.mul_comm q.den q.num

end LeanRsFixture.ProofSearchFacts
