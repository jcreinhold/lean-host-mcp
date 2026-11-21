def run (path : String) : IO String := do
  let input : Input ← IO.ofExcept <| (Json.parse (← IO.FS.readFile path)).bind fromJson?
  searchPathRef.set (input.roots.toList.map System.FilePath.mk)
  let env ← importModules (input.imports.map fun s => { module := s.toName }) {}
  let outcome ← verifyDeclarationBatch env (batch input) input.namespaceContext input.file
    input.heartbeats.toUInt64 input.diagnosticBytes.toUSize input.optionsJson
  let frame := (result outcome).compress
  if frame.utf8ByteSize + 1 > 1024 * 1024 then throw (IO.userError "verification verdict exceeds capture bound")
  return frame
end LeanHostVerify

def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged verification request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostVerify.run path)
  (← IO.getStdout).putStrLn frame
