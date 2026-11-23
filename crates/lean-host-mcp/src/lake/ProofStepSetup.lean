private structure ProofStepInput where
  source : String
  file : String
  proofPosition : ProofInput
  snippets : Array String
  perFieldBytes : Nat
  totalBytes : Nat
  namespaceContext : String
  optionsJson : String
  heartbeats : Nat
  diagnosticBytes : Nat
  deriving FromJson

private def attemptStatus : ProofAttemptStatus → String
  | .closed => "closed"
  | .progressed => "progressed"
  | .failed => "failed"
  | .timeout => "timeout"
  | .budgetExceeded => "budget_exceeded"
  | .notAttempted => "not_attempted"
  | .unsupported => "unsupported"

private def attemptRow (r : ProofAttemptRow) := obj [
  ("id", r.id), ("status", attemptStatus r.status), ("candidate_text", rendered r.candidateText),
  ("diagnostics", failure r.diagnostics), ("downstream_diagnostics", failure r.downstreamDiagnostics),
  ("goals", .arr (r.goals.map rendered)), ("declaration", toJson (r.declaration.map targetInfo)),
  ("proof_position", toJson (r.proofPosition.map fun p => obj [
    ("index", toJson p.index), ("tactic", rendered p.tactic)])),
  ("output_truncated", toJson r.outputTruncated)]

private def attemptResult (r : ProofAttemptEnvelope) := obj [
  ("candidates", .arr (r.candidates.map attemptRow)), ("candidate_limit", toJson r.candidateLimit),
  ("candidates_truncated", toJson r.candidatesTruncated), ("entry_goals", .arr (r.entryGoals.map rendered)),
  ("locals", .arr (r.locals.map localInfo))]

private def attemptOutcome : ProofAttemptOutcome → Json
  | .ok r imports => obj [("status", "ok"), ("result", attemptResult r), ("imports", toJson imports)]
  | .missingImports r imports missing => obj [("status", "missing_imports"), ("result", attemptResult r),
      ("imports", toJson imports), ("missing", toJson missing)]
  | .headerParseFailed f => obj [("status", "header_parse_failed"), ("diagnostics", failure f)]
  | .unsupported => obj [("status", "unsupported")]

unsafe def runProofStep (path : String) : IO String := do
  let i : ProofStepInput ← IO.ofExcept <| (Json.parse (← IO.FS.readFile path)).bind fromJson?
  let setup ← ModuleSetup.load "module.setup.json"
  let position ← parsePosition i.proofPosition.position
  let candidates : Array ProofCandidate := i.snippets.mapIdx fun index text => ⟨s!"candidate_{index + 1}", text⟩
  let req : ProofAttemptRequest :=
    ⟨i.source, .declaration i.proofPosition.declaration position, candidates, ⟨i.perFieldBytes, i.totalBytes⟩⟩
  Lean.enableInitializersExecution
  let outcome ← attemptProofWithSetup setup req i.namespaceContext i.file i.heartbeats.toUInt64
    i.diagnosticBytes.toUSize i.optionsJson
  return (attemptOutcome outcome).compress
end LeanHostVerify

unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged proof-step request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostVerify.runProofStep path)
  (← IO.getStdout).putStrLn frame
