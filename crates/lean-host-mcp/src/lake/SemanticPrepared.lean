unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged semantic request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostSemantic.run path fun goal => LeanSemanticSearch.Capability.proofGoalFeatures goal.compress)
  (← IO.getStdout).putStrLn frame
