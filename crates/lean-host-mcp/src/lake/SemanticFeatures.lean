import LeanSemanticSearch.Capability

/-!
One-shot semantic feature extraction for the supervised Lean reader. The caller
owns import selection and process lifetime. Requests and command responses retain
the released semantic JSON contract; opaque feature rows are never reconstructed.
Only the final bounded result goes to stdout. Source output goes to stderr, while
structured domain diagnostics remain inside their original command responses.
-/

open Lean

namespace LeanHostSemantic

private def maxInputBytes : Nat := 16 * 1024 * 1024
private def maxOutputBytes : Nat := 1024 * 1024

/-- Ordered import roots and the two existing semantic command payloads. -/
structure Input where
  roots : Array String
  goal : Json
  declarations : Json
  deriving FromJson

/-- The existing semantic envelope, leaving its package-owned rows opaque. -/
structure CommandResponse where
  schema_version : String
  command : String
  command_version : String
  feature_version : String
  rows : Array Json
  diagnostics : Array Json
  deriving FromJson, ToJson

/-- Both commands execute in the same supervised Lean process. -/
structure Result where
  goal : CommandResponse
  declarations : CommandResponse
  deriving ToJson

private def readInput (path : String) : IO Input := do
  let handle ← IO.FS.Handle.mk path .read
  let mut bytes := ByteArray.empty
  repeat
    let chunk ← handle.read ((min 65536 (maxInputBytes + 1 - bytes.size)).toUSize)
    if chunk.isEmpty then break
    bytes := bytes ++ chunk
    if bytes.size > maxInputBytes then
      throw (IO.userError "semantic request exceeds input bound")
  let some text := String.fromUTF8? bytes
    | throw (IO.userError "semantic request is not UTF-8")
  IO.ofExcept <| (Json.parse text).bind fromJson?

private def commandResponse (raw command version : String) : IO CommandResponse := do
  if raw.utf8ByteSize > maxOutputBytes then
    throw (IO.userError "semantic response exceeds capture bound")
  let response : CommandResponse ← IO.ofExcept <| (Json.parse raw).bind fromJson?
  if response.schema_version != LeanSemanticSearch.JsonSupport.schemaVersion ||
      response.command != command || response.command_version != version ||
      response.feature_version != LeanSemanticSearch.JsonSupport.semanticFeatureVersion then
    throw (IO.userError "unexpected semantic response contract")
  return response

unsafe def run (path : String) : IO String := do
  let input ← readInput path
  if input.roots.isEmpty then
    throw (IO.userError "semantic search requires explicit import roots")
  searchPathRef.set (input.roots.toList.map System.FilePath.mk)
  let goal ← commandResponse
    (← LeanSemanticSearch.Capability.proofGoalFeatures input.goal.compress)
    LeanSemanticSearch.JsonSupport.proofGoalCommand
    LeanSemanticSearch.JsonSupport.proofGoalFeatureCommandVersion
  let declarations ← commandResponse
    (← LeanSemanticSearch.Capability.declarationFeatures input.declarations.compress)
    LeanSemanticSearch.JsonSupport.declarationCommand
    LeanSemanticSearch.JsonSupport.declarationFeatureCommandVersion
  let result : Result := { goal, declarations }
  let frame := (toJson result).compress
  if frame.utf8ByteSize + 1 > maxOutputBytes then
    throw (IO.userError "semantic result exceeds capture bound")
  return frame

end LeanHostSemantic

unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged semantic request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostSemantic.run path)
  (← IO.getStdout).putStrLn frame
