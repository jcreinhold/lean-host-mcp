# Tool Catalog

`lean-host-mcp` exposes five semantic Lean tools. Each tool has a `kind` field
that selects a mode inside that job family. The surface is intentionally small:
it gives an agent proof context, safe experiments, verification, semantic
lookup, and cheap status without exposing raw Lean worker primitives.

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

Every call is read-only. The server reads files, elaborates in memory, and never
writes source.

## Response Shape

All public tools return the semantic response baseline:

```jsonc
{
  "data": { "...": "mode-specific result" },
  "errors": [],
  "trust": {
    "project_root": "/abs/project",
    "session_id": "metadata-only-or-worker-session",
    "lean_toolchain": "leanprover/lean4:v4.31.0-rc2"
  }
}
```

Lean-domain outcomes remain data. A failed tactic, a rejected declaration, an
ambiguous name, or a `needs_build` verdict is not an MCP transport error.
Infrastructure failures that the client can retry, such as worker admission
pressure or restart-loop exhaustion, appear in `errors` with structured details.
Warnings and next actions from the underlying implementation are also carried as
warning issues in `errors`.

## Typical Workflow

1. Call `lean_context` with `kind: "proof_position"` to read the current proof
   goals, locals, expected type, and diagnostics for a declaration position.
2. Call `lean_lookup` with `kind: "proof_search"` to retrieve ranked
   declarations for the goal.
3. Call `lean_lookup` with `kind: "declaration"` to inspect a promising
   declaration's statement, docstring, attributes, and flags.
4. Call `lean_trial` with `kind: "proof_step"` to try one or more tactics in
   memory without editing the file.
5. Call `lean_verify` with `kind: "explicit"` to verify the target declaration.

Use `lean_lookup` with `kind: "references"` when the task is semantic reference
discovery rather than proof search. Use `lean_status` for cheap project and host
status before spending a worker permit.

## `lean_context`

### `kind: "proof_position"`

Returns proof context for one declaration proof position. The request fields are
the existing declaration anchor plus an optional proof-position selector:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "default" }
}
```

When `proof_position` is omitted, the default is the pristine entry goal: the
state before any tactic runs. This is the same position where a default
`lean_trial(kind="proof_step")` snippet is spliced.

Other selectors are:

```json
{ "kind": "index", "index": 0 }
```

for the state after the first tactic, and:

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

for the state after a matched source fragment.

The `data` payload is the proof context result previously produced internally by
the proof-position operation: status, diagnostics, goals, locals, expected type,
truncation, and any `needs_build` or ambiguity facts.

## `lean_trial`

### `kind: "proof_step"`

Tries one or more proof snippets at a declaration proof position against an
in-memory source snapshot. It never writes files.

```json
{
  "kind": "proof_step",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippet": "trivial"
}
```

Use `snippets` to try a bounded list independently:

```json
{
  "kind": "proof_step",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "snippets": ["simp", "exact h"]
}
```

The response reports each candidate's status, diagnostics, resulting goals, and
whether the candidate set was truncated.

## `lean_verify`

### `kind: "explicit"`

Verifies one named declaration in a file, in memory.

```json
{
  "kind": "explicit",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.closedTheorem",
  "allow_sorry": false,
  "report_axioms": true
}
```

The initial mode is intentionally only a single explicit target. File-all,
module-all, and changed-target verification are later workflow modes, not part
of this baseline.

## `lean_lookup`

### `kind: "declaration"`

Inspects one declaration by name. Use `file` when local imports or namespace
context are needed; use `imports` for explicit import context.

```json
{
  "kind": "declaration",
  "name": "Nat.add_zero",
  "imports": ["Init"]
}
```

Optional `fields` can select `source`, `statement`, `docstring`, `attributes`,
and `flags`; `raw_statement` asks for the raw elaborated term.

### `kind: "proof_search"`

Returns ranked declarations relevant to a proof goal. The target can come from a
file/declaration position:

```json
{
  "kind": "proof_search",
  "file": "LeanRsFixture/ProofAgent.lean",
  "declaration": "LeanRsFixture.ProofAgent.miniRatDenominatorStep",
  "mode": "next_step",
  "limit": 10
}
```

or from explicit goal/type text:

```json
{
  "kind": "proof_search",
  "goal": "⊢ True",
  "imports": ["LeanRsFixture.SourceRanges"],
  "mode": "exact"
}
```

Modes are `next_step`, `exact`, `apply`, `rewrite`, and `simp`. `limit` is
clamped to the tool cap.

### `kind: "references"`

Finds semantic references to a fully-qualified Lean name.

File scope elaborates one anchor file through the worker, so it reflects the
current source snapshot:

```json
{
  "kind": "references",
  "name": "LeanRsFixture.ProofActions.closedTheorem",
  "scope": "file",
  "file": "LeanRsFixture/ProofActions.lean",
  "limit": 20
}
```

Project scope reads the on-disk `.ilean` reference index and does not open a
worker:

```json
{
  "kind": "references",
  "name": "LeanRsFixture.ProofSearchFacts.MiniRat",
  "scope": "project",
  "files": ["LeanRsFixture/ProofSearchFacts.lean"],
  "limit": 100
}
```

## `lean_status`

### `kind: "project"`

Returns cheap project, toolchain, output, broker, and admission configuration.
This mode uses Lake metadata only and does not open a worker or consume a
semantic permit.

```json
{ "kind": "project" }
```

Use `project` to override the default Lake root:

```json
{ "kind": "project", "project": "/abs/path/to/lake/project" }
```

Prompt 73 extends this status surface with typed trust and artifact facts.

## Maintainer Migration Table

The old public tools are no longer registered. Existing implementation code is
still reused internally behind the semantic modes.

| Old public tool | New public tool and mode |
| --- | --- |
| `proof_state` | `lean_context`, `kind: "proof_position"` |
| `try_proof_step` | `lean_trial`, `kind: "proof_step"` |
| `verify_declaration` | `lean_verify`, `kind: "explicit"` |
| `inspect_declaration` | `lean_lookup`, `kind: "declaration"` |
| `search_for_proof` | `lean_lookup`, `kind: "proof_search"` |
| `find_references` | `lean_lookup`, `kind: "references"` |

Do not re-add compatibility aliases for the old names.
