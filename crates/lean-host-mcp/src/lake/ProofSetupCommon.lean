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

private def parsePosition (input : PositionInput) : IO ProofPositionSelector := do
  match input.kind, input.index, input.text with
    | "entry", none, none => pure ProofPositionSelector.entry
    | "index", some index, none => pure (.index index)
    | "after_text", none, some text => pure (.afterText text input.occurrence)
    | _, _, _ => throw (IO.userError "invalid staged proof position")

private def rendered (r : RenderedInfo) := obj [("value", r.value), ("truncated", toJson r.truncated)]

private def localInfo (l : LocalInfo) := obj [
  ("name", l.name), ("binder_info", l.binderInfo), ("type_str", rendered l.typeStr),
  ("value", toJson (l.value.map rendered))]

