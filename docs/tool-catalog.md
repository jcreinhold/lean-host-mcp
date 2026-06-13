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
    "lean_toolchain": "leanprover/lean4:v4.31.0-rc2",
    "artifacts": [
      {
        "artifact": "ilean",
        "scope": "project",
        "status": "build_fresh",
        "detail": "project reference index is current for contributing modules"
      }
    ]
  }
}
```

`trust.artifacts` is omitted when empty. Rows use these stable tokens:

- `artifact`: `source`, `olean`, `ilean`, `worker`
- `scope`: `file`, `module`, `project`, `toolchain`
- `status`: `edit_fresh`, `build_fresh`, `stale_build`, `missing_build`, `unknown`, `not_applicable`

Rows may also carry `path`, `module`, `detail`, and `next_action`. Source-overlay tools and source-backed declaration
inventory report the source file snapshot as `source` / `file` / `edit_fresh`; project reference lookup and index-backed
declaration inventory report `.ilean` build freshness or missing/stale build state; `needs_build` degradations report
missing `.olean` artifacts. Runtime counters, cache timings, and import lists remain telemetry, not trust.

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
5. Call `lean_verify` with an explicit target group to verify the target declaration.

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

Verifies declarations in memory. Targets can be explicit declaration lists,
every declaration in a file, or every declaration in a module. The server reads
Lean source and calls Lean's elaborator/kernel through the worker; it does not
run `lake build`.

Single declaration:

```json
{
  "targets": [
    {
      "kind": "explicit",
      "file": "LeanRsFixture/ProofActions.lean",
      "declarations": ["LeanRsFixture.ProofActions.closedTheorem"]
    }
  ],
  "allow_sorry": false,
  "report_axioms": true
}
```

Mixed target groups:

```json
{
  "targets": [
    {
      "kind": "explicit",
      "file": "LeanRsFixture/ProofActions.lean",
      "declarations": [
        "LeanRsFixture.ProofActions.closedTheorem",
        "LeanRsFixture.ProofActions.sorryTheorem"
      ]
    },
    { "kind": "file_all", "file": "LeanRsFixture/ProofAgent.lean" },
    { "kind": "module_all", "module": "LeanRsFixture.ProofActions" }
  ],
  "allow_sorry": false,
  "report_axioms": false
}
```

The response is a compact batch:

```json
{
  "summary": {
    "requested": 4,
    "verified": 3,
    "failed": 1,
    "needs_build": 0,
    "truncated": false
  },
  "results": [
    {
      "id": "group_1:LeanRsFixture.ProofActions.closedTheorem",
      "file": "LeanRsFixture/ProofActions.lean",
      "declaration": "LeanRsFixture.ProofActions.closedTheorem",
      "verification_status": "verified",
      "facts": {}
    }
  ]
}
```

`verification_status` uses the same vocabulary as the declaration-verification
projection: `verified`, `has_sorry`, `has_unresolved_goals`,
`has_diagnostics`, `not_found`, `ambiguous`, `needs_build`, `timeout`,
`budget_exceeded`, `worker_recycled`, or `unsupported`. `requested` counts
expanded targets before host-side caps; `truncated` is true when declaration
inventory or verification output was capped. `file_all` and source-backed
`module_all` use the current source snapshot. If a module has no source file,
`module_all` may use the `.ilean` declaration inventory, with typed artifact
freshness facts in `trust`.

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

### `kind: "declarations"`

Lists declarations in one source file or module. This is declaration inventory,
so it is a semantic lookup mode rather than a separate public tool.

File targets read the current source snapshot and use the worker declaration
outline selector:

```json
{
  "kind": "declarations",
  "target": { "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" },
  "limit": 200
}
```

Module targets first resolve `<module>.lean` under the project root and use the
same source-fresh path when the file exists:

```json
{
  "kind": "declarations",
  "target": { "kind": "module", "module": "LeanRsFixture.ProofAgent" },
  "limit": 200
}
```

If a module source file is unavailable but a matching `.ilean` exists, the mode
returns build-fresh index rows instead. Index rows know the declaration range
and name/selection range but not the declaration kind or body span, so `kind`
and `body_span` are omitted. If neither source nor index is available, the
result status is `missing_build` or `not_found`, never an empty successful list.
`limit` defaults to 200 and is capped at 1000; truncation keeps a deterministic
prefix and sets `truncated: true`.

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

Use `include` to request cheap status sections. The default is all sections:

```json
{
  "kind": "project",
  "project": "/abs/path/to/lake/project",
  "include": ["toolchain", "worker", "artifacts"]
}
```

`lean_status` reads Lake metadata and cheap filesystem facts only: it does not run `lake`, does not read source files,
and does not open a worker. When the project build tree is absent it reports `olean` and `ilean` `missing_build` facts;
when the build tree exists but no semantic query has checked source mtimes it reports artifact freshness as `unknown`.
Worker runtime generation is likewise `not_applicable` because this status mode deliberately avoids opening a worker.

## Maintainer Migration Table

The old public tools are no longer registered. Existing implementation code is
still reused internally behind the semantic modes.

| Old public tool | New public tool and mode |
| --- | --- |
| `proof_state` | `lean_context`, `kind: "proof_position"` |
| `try_proof_step` | `lean_trial`, `kind: "proof_step"` |
| `verify_declaration` | `lean_verify` with one `kind: "explicit"` target group |
| `inspect_declaration` | `lean_lookup`, `kind: "declaration"` |
| `search_for_proof` | `lean_lookup`, `kind: "proof_search"` |
| `find_references` | `lean_lookup`, `kind: "references"` |

Do not re-add compatibility aliases for the old names.
