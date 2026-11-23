structure OutlineInput where
  proofPosition : Option Json := none
  source : String
  file : String
  perFieldBytes : Nat
  totalBytes : Nat
  namespaceContext : String
  optionsJson : String
  heartbeats : Nat
  diagnosticBytes : Nat
  deriving FromJson

private def outlineItem : ModuleQueryBatchItem → IO Json
  | .ok id (.declarationOutline r) => pure <| obj [
      ("status", "ok"), ("id", id), ("result", obj [("result", "declaration_outline"),
        ("body", obj [("declarations", .arr (r.declarations.map targetInfo)), ("truncated", toJson r.truncated)])])]
  | .ok id (.diagnostics f) => pure <| obj [
      ("status", "ok"), ("id", id), ("result", obj [("result", "diagnostics"), ("body", failure f)])]
  | .unavailable id message => pure <| obj [("status", "unavailable"), ("id", id), ("message", message)]
  | .budgetExceeded id message => pure <| obj [("status", "budget_exceeded"), ("id", id), ("message", message)]
  | item => match extraItem item with
    | some value => pure value
    | none => throw (IO.userError "unexpected supervised file selector")

private def outlineEnvelope (e : ModuleQueryBatchEnvelope) : IO Json := do
  return obj [("items", .arr (← e.items.mapM outlineItem)), ("total_truncated", toJson e.totalTruncated)]

unsafe def runOutline (path : String) : IO String := do
  let i : OutlineInput ← IO.ofExcept <| (Json.parse (← IO.FS.readFile path)).bind fromJson?
  let setup ← ModuleSetup.load "module.setup.json"
  Lean.enableInitializersExecution
  let outcome ← projectFile i.proofPosition setup i.source ⟨i.perFieldBytes, i.totalBytes⟩
    i.namespaceContext i.file i.heartbeats.toUInt64 i.diagnosticBytes.toUSize i.optionsJson
  let value ← match outcome with
    | .ok e imports => pure <| obj [("status", "ok"), ("result", ← outlineEnvelope e), ("imports", toJson imports)]
    | .missingImports e imports missing => pure <| obj [("status", "missing_imports"), ("result", ← outlineEnvelope e),
        ("imports", toJson imports), ("missing", toJson missing)]
    | .headerParseFailed f => pure <| obj [("status", "header_parse_failed"), ("diagnostics", failure f)]
  let frame := value.compress
  if frame.utf8ByteSize + 1 > 1024 * 1024 then throw (IO.userError "supervised file result exceeds capture bound")
  return frame
end LeanHostVerify

unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged outline request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostVerify.runOutline path)
  (← IO.getStdout).putStrLn frame
