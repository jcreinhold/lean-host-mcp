unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged semantic request")
  let frame ← IO.withStdout (← IO.getStderr) do
    let setup ← ModuleSetup.load "module.setup.json"
    LeanHostSemantic.run path fun goal => do
      let response ← LeanSemanticSearch.GoalElaboration.runWithSetup setup goal
      let command := LeanSemanticSearch.JsonSupport.proofGoalCommand
      let version := LeanSemanticSearch.JsonSupport.proofGoalFeatureCommandVersion
      let features := LeanSemanticSearch.JsonSupport.semanticFeatureVersion
      return (match response with
        | .ok rows => LeanSemanticSearch.JsonSupport.responseJson command version features rows #[]
        | .error err => LeanSemanticSearch.JsonSupport.errorResponseJson command version features err).compress
  (← IO.getStdout).putStrLn frame
