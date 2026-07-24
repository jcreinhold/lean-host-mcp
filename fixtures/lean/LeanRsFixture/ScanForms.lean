/-! Declaration-candidate scan forms: surface syntax the shim's candidate
    scan historically dropped (multi-clause equation `def`s and theorems,
    `where`-structure defs, `structure`/`class` commands, anonymous
    `instance`s). The lean-host-mcp fixture e2e verifies each resolves by
    name instead of returning `not_found`. -/

namespace LeanRsFixture.ScanForms

/-- A two-field structure. -/
structure Point where
  x : Nat
  y : Nat

/-- A single-field class. -/
class Default (α : Type) where
  value : α

/-- Multi-clause equation definition. -/
def multi : Nat → Nat
  | 0 => 0
  | k + 1 => multi k + 1

/-- Multi-clause equation theorem. -/
theorem zeroOrSucc : ∀ n : Nat, n = 0 ∨ ∃ k, n = k + 1
  | 0 => Or.inl rfl
  | k + 1 => Or.inr ⟨k, rfl⟩

/-- `where`-structure definition. -/
def origin : Point where
  x := 0
  y := 0

/-- Anonymous instance: resolves under its generated `inst…` name. -/
instance : Default Point := ⟨origin⟩

/-- Named-instance control. -/
instance namedDefault : Default Nat := ⟨0⟩

end LeanRsFixture.ScanForms
