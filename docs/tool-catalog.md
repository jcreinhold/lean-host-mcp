# Tool Catalog

`lean-host-mcp` exposes a small, declaration-centric proof-agent surface. You name a declaration — and, when it matters,
a position inside its proof — and the tools read the goal state, retrieve relevant lemmas, inspect declarations, test
tactics, and verify results. There are no raw span, hover, type-at, source-grep, or arbitrary-query tools; the surface
is the proof workflow, not a mirror of Lean's internals.

```text
proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration
```

`find_references` is the companion semantic-lookup tool.

## Typical workflow

A proof agent usually moves left to right along that arrow:

1. **`proof_state`** to read what's left to prove at the current position — the open *goals* (the propositions still to
   be shown), the local hypotheses, and the expected type.
2. **`search_for_proof`** to get a ranked list of declarations that might close or advance the goal. Each row is
   lightweight metadata only.
3. **`inspect_declaration`** on a promising candidate to read its full statement, docstring, and attributes before
   committing to it.
4. **`try_proof_step`** to run a candidate tactic at the position and see whether it closes the goal or what new goals
   it leaves — all in memory, without editing the file.
5. **`verify_declaration`** to confirm the whole declaration type-checks, optionally rejecting `sorry` or reporting the
   axioms it depends on.

Every response is wrapped in the shared envelope (`status`, `result`, `freshness`, `runtime`, …) described in the
[README](../README.md#response-envelope). Lean-domain outcomes — a failed tactic, a rejected proof, a missing import —
are part of a successful (`status: "ok"`) result. A full project mailbox or a restart-loop failure is reported as a
retryable `runtime_unavailable` response, documented in [`operations.md`](operations.md#runtime-error-contract).

## `proof_state`

The proof context at one position inside a declaration: open goals, local hypotheses, expected type, and diagnostics. A
*proof position* is a point in the tactic block; a *tactic state* is the set of goals open there.

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem"
}
```

`proof_position` is optional. When omitted, the worker selects the first open tactic state in declaration order. To
target a specific one, select it by index:

```json
{ "kind": "index", "index": 1 }
```

or by the source text just before it:

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

A successful response carries `status: "context"` and the context itself:

```jsonc
{
  "status": "context",
  "declaration_name": "LeanRsFixture.ProofActions.stepTheorem",
  "goals_before": ["⊢ p ∧ q"],          // goals open at this position
  "goals_after":  ["⊢ q"],               // goals after the position's tactic, when applicable
  "locals": [ { "name": "h", "type_str": { "value": "p", "truncated": false }, "value": null } ],
  "expected_type": { "value": "p ∧ q", "truncated": false },
  "truncated": false,
  "query_facts": { "cache_status": "hit", "output_bytes": 412 /* … */ }
}
```

A header that doesn't parse returns `status: "header_parse_failed"`; a worker without the optional module-query shim
returns `status: "unsupported"`. The source spans in responses are diagnostic evidence, not edit handles. When a
selector cannot resolve because the project build is incomplete, it appears in a `needs_build` array (distinct from
`unavailable`) and a top-level warning names the `lake build` cue — see
[The `needs_build` signal](#the-needs_build-signal).

## `search_for_proof`

Ranked declarations relevant to the next proof step. Give it a declaration target when you have a file:

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "mode": "next_step",
  "limit": 10
}
```

or explicit goal/type text when you don't:

```json
{
  "goal": "⊢ True",
  "imports": ["LeanRsFixture.SourceRanges"],
  "mode": "exact"
}
```

`mode` shapes the ranking and the suggested snippet — `next_step` (default), `exact`, `apply`, `rewrite`, or `simp`.
`limit` is clamped to 1–20 (default 10). Rows carry bounded metadata only — name, kind, module, source, a relevance
`score`, a `match_reason`, and a ready-to-paste `suggested_snippet` — not full statements. Inspect a chosen candidate by
name with `inspect_declaration` to read its statement or attributes.

```jsonc
{
  "declarations": [
    {
      "name": "And.intro",
      "kind": "theorem",
      "module": "Init.Core",
      "score": 87,
      "rank": 0,
      "match_reason": "conclusion_head",
      "source": { "file": "…", "start_line": 1, "start_column": 0, "end_line": 1, "end_column": 0 }
    }
    // … up to `limit` rows, ranked best-first
  ],
  "truncated": false,
  "facts": { "declarations_scanned": 1840, "timings": { /* … */ } }
}
```

## `inspect_declaration`

Everything known about one declaration, by name: source location, statement, docstring, attributes, and flags.

```json
{
  "name": "Nat.add_zero",
  "imports": ["Mathlib.Data.Nat.Basic"],
  "fields": ["statement", "attributes"]
}
```

`fields` selects which parts to return (all default on). For a project declaration, pass `file` to derive the local
imports needed to resolve it:

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "file": "LeanRsFixture/SourceRanges.lean"
}
```

`statement` is pretty-printed by default (notation-aware, `pp.universes false`) — an editor-`hover`-quality signature
rather than a fully-elaborated term. `statement_rendering` reports the path that produced it (`"pretty"`, or `"raw"`
when the pretty-printer fell back). Set `"raw_statement": true` for the fully-elaborated `Expr.toString` form.

The result is one of `found`, `not_found`, or `unsupported`. A `found` declaration looks like:

```jsonc
{
  "status": "found",
  "declaration": {
    "name": "Nat.add_zero",
    "kind": "theorem",
    "module": "Init.Data.Nat.Basic",
    "statement": { "value": "∀ (n : Nat), n + 0 = n", "truncated": false },
    "statement_rendering": "pretty",
    "docstring": { "value": "…", "truncated": false },
    "attributes": ["simp"],
    "flags": { "is_private": false, "is_generated": false, "is_internal": false }
  }
}
```

Rendered text fields always carry their own `truncated` flag, since output is capped before crossing the worker
boundary.

## `try_proof_step`

Runs one or more candidate tactics at a proof position and reports what each does — in an in-memory overlay. **It never
writes source files.** Apply a snippet you like yourself.

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippet": "trivial"
}
```

Pass `snippets` (a list) to try several at once; up to 8 candidates are attempted, the rest reported as
`budget_exceeded`. Each candidate reports its status (`closed`, `progressed`, `failed`, `timeout`, …), the diagnostics
it produced, the goals it leaves behind, and the resolved declaration/position it ran against:

```jsonc
{
  "status": "ok",
  "result": {
    "candidates": [
      {
        "id": "0",
        "status": "closed",                 // this snippet closed the goal
        "snippet": { "value": "trivial", "truncated": false },
        "goals": [],                          // none left
        "diagnostics": { "diagnostics": [], "truncated": false }
      }
    ],
    "candidate_limit": 8,
    "candidates_truncated": false
  }
}
```

## `verify_declaration`

Elaborates an in-memory snapshot and checks whether one declaration type-checks, under the requested `sorry`/axiom
policy. **It never writes source files.**

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "allow_sorry": false,
  "report_axioms": true
}
```

`verification_status` is the verdict — `verified`, `has_sorry`, `has_unresolved_goals`, `has_diagnostics`, `not_found`,
`needs_build`, `ambiguous`, and so on. A policy failure (e.g. a `sorry` when `allow_sorry` is false) is a normal
structured result, not an infrastructure error.

`needs_build` means the name could not be resolved against a complete project environment — usually because the project
is not fully built — so the facts were computed against a degraded environment. `ambiguous` means the name genuinely
resolves to more than one declaration; `facts.candidates` names the competitors (fully-qualified) so you can
disambiguate. In both cases `facts.facts_trustworthy` is `false` and a top-level `warning` carries the cue (`lake build`
for `needs_build`, fully-qualify for `ambiguous`); it is `true` only for clean verdicts that checked a resolved target.
Do not read a clean `contains_sorry:false` / `unresolved_goals:[]` as "verified" when `facts_trustworthy` is `false`.

`facts.axioms_available` distinguishes "checked, no nontrivial axioms" (`true` with empty `axioms`) from "could not
check" (`false` — target unresolved or budget exhausted); when `false` and `report_axioms` was requested, a warning says
the axiom list is not authoritative.

```jsonc
{
  "status": "ok",
  "result": {
    "status": "ok",
    "verification_status": "verified",
    "facts": {
      "contains_sorry": false,
      "unresolved_goals": [],
      "axioms": ["propext", "Classical.choice", "Quot.sound"],  // when report_axioms is true
      "axioms_available": true,
      "diagnostics": { "diagnostics": [], "truncated": false },
      "facts_trustworthy": true
    }
  }
}
```

## `find_references`

Semantic — not textual — lookup of every use of a fully-qualified name. Binders are reported as `kind: "def"`, uses as
`kind: "ref"`. Scope is one file:

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "scope": "file",
  "file": "LeanRsFixture/SourceRanges.lean"
}
```

or the project, optionally narrowed to an explicit file list and a `limit` (capped at 1000):

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "scope": "project",
  "files": ["LeanRsFixture/SourceRanges.lean"],
  "limit": 20
}
```

```jsonc
{
  "status": "ok",
  "references": [
    { "file": "…/SourceRanges.lean", "line": 12, "column": 8, "end_line": 12, "end_column": 20, "kind": "def" },
    { "file": "…/Uses.lean",         "line": 7,  "column": 4, "end_line": 7,  "end_column": 16, "kind": "ref" }
  ],
  "truncated": false,
  "files_scanned": 2,
  "files_skipped": 0
}
```

Project scope also reports header-parse failures, files with missing imports, and unsupported files, so a partial scan
is visible rather than silent. If a file's query hits a missing `.olean` (an unbuilt dependency), project scope **does
not** fail the whole request: it skips that file, continues — returning the references indexed so far, like an editor's
"indexed so far" — and attaches a top-level `needs_build` warning naming the blocking `lake build` cue.

## The `needs_build` signal

Several tools share one honest verdict for "the project environment is incomplete." `verify_declaration` reports it as
`verification_status: "needs_build"`; `proof_state` collects the affected selectors in a `needs_build` array (distinct
from `unavailable`); `find_references` and `search_for_proof` surface it as a top-level warning. In every case the
warning text and a `lake build` next action are identical. The fix is always the same: run `lake build`, resolve errors,
then retry.

This replaces what used to be a misleading `"ambiguous"` verdict with no candidates, or a hard error. The worker now
classifies resolution at its own boundary, so `"ambiguous"` is reserved for *genuine* multiple-resolution and always
arrives with the competing declarations named (`proof_state`'s `ambiguous` array, `verify_declaration`'s
`facts.candidates`), with a fully-qualify next action.
