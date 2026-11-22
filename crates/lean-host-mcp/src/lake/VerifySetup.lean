unsafe def run (path : String) : IO String := do
  let input : Input ← IO.ofExcept <| (Json.parse (← IO.FS.readFile path)).bind fromJson?
  if verifySetupReportRuntime && input.expectedRuntimeVersion != some Lean.versionString then
    throw (IO.userError "executing Lean version differs from the selected toolchain")
  let setup ← ModuleSetup.load "module.setup.json"
  Lean.enableInitializersExecution
  let outcome ← verifySetup setup (batch input) input.namespaceContext input.file
    input.heartbeats.toUInt64 input.diagnosticBytes.toUSize input.optionsJson
  let verdict := result outcome
  let frame := (if verifySetupReportRuntime then
    obj [("runtime_lean_version", toJson Lean.versionString), ("outcome", verdict)]
    else verdict).compress
  if frame.utf8ByteSize + 1 > 1024 * 1024 then throw (IO.userError "verification verdict exceeds capture bound")
  return frame
end LeanHostVerify

unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged verification request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostVerify.run path)
  (← IO.getStdout).putStrLn frame
