import LeanRsHostShims.InfoTree

open Lean LeanRsFixture.InfoTree
open LeanRsFixture.Elaboration

namespace LeanHostVerify

private def obj := Json.mkObj

private def span (s : ModuleSourceSpan) := obj [
  ("start_line", toJson s.startLine), ("start_column", toJson s.startColumn),
  ("end_line", toJson s.endLine), ("end_column", toJson s.endColumn)]

private def targetInfo (t : DeclarationTargetInfo) := obj [
  ("short_name", t.shortName), ("declaration_name", t.declarationName),
  ("namespace_name", t.namespaceName), ("declaration_kind", t.declarationKind),
  ("declaration_span", span t.declarationSpan), ("name_span", span t.nameSpan),
  ("body_span", span t.bodySpan)]

private def diagnostic (d : Diagnostic) := obj [
  ("severity", toJson (match d.severity with | .info => "info" | .warning => "warning" | .error => "error")),
  ("message", d.message), ("file_label", d.fileLabel), ("coordinate_space", "original_source"),
  ("original_range", toJson (d.position.map fun p => obj [
    ("file", d.fileLabel), ("start_line", toJson p.line), ("start_column", toJson p.column),
    ("end_line", toJson (p.endLine.getD p.line)), ("end_column", toJson (p.endColumn.getD p.column))])) ,
  ("line", toJson (d.position.map (·.line))), ("column", toJson (d.position.map (·.column))),
  ("end_line", toJson (d.position.bind (·.endLine))), ("end_column", toJson (d.position.bind (·.endColumn)))]

private def failure (f : ElabFailure) := obj [
  ("diagnostics", .arr (f.diagnostics.map diagnostic)),
  ("truncated", toJson (match f.truncated with | .complete => false | .truncated => true))]

private def row (r : DeclarationVerificationBatchRow) : Json := Id.run do
  let f := r.facts
  let status := match r.status with
    | .accepted => "accepted" | .rejected => "rejected" | .notFound => "not_found"
    | .ambiguous => "ambiguous" | .timeout => "timeout" | .budgetExceeded => "budget_exceeded"
    | .unsupported => "unsupported" | .needsBuild => "needs_build"
  let target := match r.target with
    | .name n => obj [("kind", "name"), ("name", n)]
    | .span s => obj [("kind", "span"), ("span", span s)]
  return obj [("id", r.id), ("target", target), ("verification_status", status),
    ("facts", obj [("target", toJson (f.target.map targetInfo)), ("diagnostics", failure f.diagnostics),
      ("unresolved_goals", .arr (f.unresolvedGoals.map fun g => obj [("value", g.value), ("truncated", toJson g.truncated)])),
      ("contains_sorry", toJson f.containsSorry), ("contains_admit", toJson f.containsAdmit),
      ("contains_sorry_ax", toJson f.containsSorryAx), ("axioms", toJson f.axioms),
      ("axioms_available", toJson f.axiomsAvailable), ("axioms_truncated", toJson f.axiomsTruncated),
      ("output_truncated", toJson f.outputTruncated), ("candidates", .arr (f.candidates.map targetInfo))])]

private def result : DeclarationVerificationBatchOutcome → Json
  | .ok rs imports => obj [("status", "ok"), ("results", .arr (rs.map row)), ("imports", toJson imports)]
  | .missingImports rs imports missing => obj [("status", "missing_imports"), ("results", .arr (rs.map row)),
      ("imports", toJson imports), ("missing", toJson missing)]
  | .headerParseFailed ds => obj [("status", "header_parse_failed"), ("diagnostics", failure ds)]
  | .unsupported => obj [("status", "unsupported")]

structure NamedTarget where
  id : String
  name : String
  deriving FromJson

structure Input where
  source : String
  file : String
  roots : Array String
  imports : Array String
  targets : Array NamedTarget
  allowSorry : Bool
  reportAxioms : Bool
  perFieldBytes : Nat
  totalBytes : Nat
  namespaceContext : String
  optionsJson : String
  heartbeats : Nat
  diagnosticBytes : Nat
  deriving FromJson

private def batch (input : Input) : DeclarationVerificationBatchRequest :=
  {
    source := input.source, targets := input.targets.map fun t => ⟨t.id, .name t.name⟩
    sorryPolicy := if input.allowSorry then 0 else 1
    reportAxioms := if input.reportAxioms then 1 else 0
    budgets := ⟨input.perFieldBytes, input.totalBytes⟩ }
