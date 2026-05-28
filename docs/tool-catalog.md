# Tool Catalog

`lean-host-mcp` exposes a small declaration-centric proof-agent surface:

```text
proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration
```

`find_references` is the companion semantic lookup tool. There are no public raw span, hover, type-at, source-grep,
placement, or arbitrary query tools.

## `proof_state`

Return the proof context for one declaration proof position.

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem"
}
```

`proof_position` is optional:

```json
{ "kind": "index", "index": 1 }
```

or:

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

When omitted, the worker selects the first open tactic state in declaration order. Source spans in responses are
diagnostic evidence only; they are not public edit handles.

## `search_for_proof`

Return ranked declaration candidates for the next proof step. Use a declaration target when a file is available:

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "mode": "next_step",
  "limit": 10
}
```

Use explicit text when no file context is available:

```json
{
  "goal": "⊢ True",
  "imports": ["LeanRsFixture.SourceRanges"],
  "mode": "exact"
}
```

Rows contain bounded metadata only. Inspect one selected candidate by name to get statement text or attributes.

## `inspect_declaration`

Inspect one declaration by name.

```json
{
  "name": "Nat.add_zero",
  "imports": ["Mathlib.Data.Nat.Basic"],
  "fields": ["statement", "attributes"]
}
```

`file` may be supplied to derive local imports for project declarations:

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "file": "LeanRsFixture/SourceRanges.lean"
}
```

Rendered text fields always carry `truncated`.

## `try_proof_step`

Try one or more tactic fragments at a selected proof position in memory. The tool never writes source files.

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippet": "trivial"
}
```

Candidate rows report status, local diagnostics, downstream diagnostics, resulting goals, the resolved declaration, and
the selected proof-position summary.

## `verify_declaration`

Verify one declaration in an in-memory source snapshot under sorry/axiom policy.

```json
{
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "allow_sorry": false,
  "report_axioms": true
}
```

Policy failures are normal structured results, not MCP infrastructure errors.

## `find_references`

Find semantic references to a fully-qualified Lean name.

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "scope": "file",
  "file": "LeanRsFixture/SourceRanges.lean"
}
```

Project scope is bounded and may use an explicit file list:

```json
{
  "name": "LeanRsFixture.SourceRanges.knownTheorem",
  "scope": "project",
  "files": ["LeanRsFixture/SourceRanges.lean"],
  "limit": 20
}
```
