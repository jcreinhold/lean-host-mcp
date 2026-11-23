private structure PositionInput where
  kind : String
  index : Option Nat := none
  text : Option String := none
  occurrence : Option Nat := none
  deriving FromJson

private structure ProofInput where
  declaration : String
  position : PositionInput
  deriving FromJson

private def projectFile (raw : Option Json) (setup : ModuleSetup) (source : String)
    (budgets : ModuleQueryOutputBudgets) (namespaceContext fileLabel : String)
    (heartbeats : UInt64) (diagBytes : USize) (optionsJson : String) : IO ModuleQueryBatchOutcome := do
  let some json := raw | throw (IO.userError "missing proof position")
  let input : ProofInput ← IO.ofExcept (fromJson? json)
  let position ← match input.position.kind, input.position.index, input.position.text with
    | "entry", none, none => pure ProofPositionSelector.entry
    | "index", some index, none => pure (.index index)
    | "after_text", none, some text => pure (.afterText text input.position.occurrence)
    | _, _, _ => throw (IO.userError "invalid staged proof position")
  fileProofStateWithSetup setup source input.declaration position budgets namespaceContext fileLabel
    heartbeats diagBytes optionsJson

private def rendered (r : RenderedInfo) := obj [("value", r.value), ("truncated", toJson r.truncated)]

private def localInfo (l : LocalInfo) := obj [
  ("name", l.name), ("binder_info", l.binderInfo), ("type_str", rendered l.typeStr),
  ("value", toJson (l.value.map rendered))]

private def boundary (b : ProofBoundaryCandidate) := obj [
  ("index", toJson b.index), ("kind", b.kind), ("source", span b.source), ("excerpt", rendered b.excerpt)]

private def proofStateJson : ProofStateResult → Json
  | .state i => obj [("status", "state"), ("info", obj [
      ("declaration_name", toJson i.declarationName), ("namespace_name", i.namespaceName),
      ("safe_edit", toJson (i.safeEdit.map targetInfo)), ("span", span i.span),
      ("goals_before", toJson i.goalsBefore), ("goals_after", toJson i.goalsAfter),
      ("locals", .arr (i.locals.map localInfo)), ("expected_type", toJson (i.expectedType.map rendered)),
      ("truncated", toJson i.truncated), ("proof_boundaries", .arr (i.proofBoundaries.map boundary)),
      ("proof_boundaries_truncated", toJson i.proofBoundariesTruncated)])]
  | .unavailable message boundaries cut => obj [("status", "unavailable"), ("message", message),
      ("proof_boundaries", .arr (boundaries.map boundary)), ("proof_boundaries_truncated", toJson cut)]
  | .ambiguous candidates => obj [("status", "ambiguous"), ("candidates", .arr (candidates.map targetInfo))]
  | .needsBuild missing => obj [("status", "needs_build"), ("missing", toJson missing)]

private def extraItem : ModuleQueryBatchItem → Option Json
  | .ok id (.proofState state) => some <| obj [
      ("status", "ok"), ("id", id), ("result", obj [("result", "proof_state"), ("body", proofStateJson state)])]
  | _ => none
