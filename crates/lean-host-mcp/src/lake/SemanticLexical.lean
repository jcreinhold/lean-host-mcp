import LeanRsHostShims.Environment

open Lean LeanRsFixture.Environment

namespace LeanHostLexical

private structure SearchBias where
  scope : String
  «prefix» : String
  strict : Bool
  weight : Int
  deriving FromJson

private structure Filter where
  include_private : Bool
  include_generated : Bool
  include_internal : Bool
  deriving FromJson

private structure Query where
  name_fragment : Option String := none
  name_match : String
  kind : Option String := none
  required_constants : Array String
  conclusion_head : Option String := none
  scope_biases : Array SearchBias
  limit : Nat
  filter : Filter
  include_source : Bool
  deriving FromJson

private structure Input where
  roots : Array String
  imports : Array String
  sourceRoots : Array String
  queries : Array Query
  deriving FromJson

private def bit (value : Bool) : Nat := if value then 1 else 0
private def obj := Json.mkObj

private def request (query : Query) : DeclarationSearchRequest := {
  nameFragment := query.name_fragment
  nameMatch := bit (query.name_match == "suffix")
  kind := query.kind
  requiredConstants := query.required_constants
  conclusionHead := query.conclusion_head
  scopeBiases := query.scope_biases.map fun b => ⟨bit (b.scope == "module"), b.«prefix», bit b.strict, toString b.weight⟩
  limit := query.limit
  filter := ⟨bit query.filter.include_private, bit query.filter.include_generated, bit query.filter.include_internal⟩
  includeSource := bit query.include_source
}

private def sourceRange (range : SourceRange) := obj [
  ("file", range.file), ("start_line", toJson range.startLine), ("start_column", toJson range.startColumn),
  ("end_line", toJson range.endLine), ("end_column", toJson range.endColumn)]

private def row (row : DeclarationSearchRow) := obj [
  ("name", row.name), ("kind", row.kind), ("module", toJson row.module),
  ("source", toJson (row.source.map sourceRange)), ("match_reason", row.matchReason),
  ("score", toJson (row.score.toInt?.getD 0)), ("rank", toJson row.rank),
  ("flags", obj [("is_private", toJson (row.flags.isPrivate != 0)),
    ("is_generated", toJson (row.flags.isGenerated != 0)), ("is_internal", toJson (row.flags.isInternal != 0))])]

private def encode (result : DeclarationSearchResult) := Id.run do
  let f := result.facts
  return obj [("declarations", .arr (result.declarations.map row)), ("truncated", toJson (result.truncated != 0)),
    ("facts", obj [("declarations_scanned", toJson f.declarationsScanned),
      ("after_name_filter", toJson f.afterNameFilter), ("after_kind_filter", toJson f.afterKindFilter),
      ("after_required_constants_filter", toJson f.afterRequiredConstantsFilter),
      ("after_conclusion_filter", toJson f.afterConclusionFilter), ("after_scope_filter", toJson f.afterScopeFilter),
      ("source_lookups", toJson f.sourceLookups), ("truncated", toJson (f.truncated != 0)),
      ("broad_pruning", .arr (f.broadPruning.map fun p => obj [
        ("stage", p.stage), ("reason", p.reason), ("count", toJson p.count)])),
      ("timings", obj [("scan_micros", toJson f.timings.scanMicros),
        ("rank_micros", toJson f.timings.rankMicros), ("source_micros", toJson f.timings.sourceMicros)])])]

unsafe def run (path : String) : IO String := do
  let i : Input ← IO.ofExcept <| (Json.parse (← IO.FS.readFile path)).bind fromJson?
  unless i.queries.size <= 6 && !i.roots.isEmpty && !i.imports.isEmpty do
    throw (IO.userError "invalid staged lexical search")
  searchPathRef.set (i.roots.toList.map System.FilePath.mk)
  let setup ← ModuleSetup.load "module.setup.json"
  Lean.enableInitializersExecution
  setup.dynlibs.forM Lean.loadDynlib
  let imports := i.imports.map fun name => { module := name.toName }
  let env ← importModules imports {} (plugins := setup.plugins) (loadExts := true) (arts := setup.importArts)
  let rows ← i.queries.mapM fun query => envSearchDeclarations env (request query) i.sourceRoots
  return (Json.arr (rows.map encode)).compress

end LeanHostLexical

unsafe def main (args : List String) : IO Unit := do
  let [path] := args | throw (IO.userError "expected one staged lexical request")
  let frame ← IO.withStdout (← IO.getStderr) (LeanHostLexical.run path)
  (← IO.getStdout).putStrLn frame
