# Tool Catalog

`lean-host-mcp` exposes five semantic Lean tools. Four tools use a `kind` field
to select a mode inside that job family: `lean_context`, `lean_trial`,
`lean_lookup`, and `lean_status`. `lean_verify` takes target groups directly.
The surface is intentionally small: it gives an agent proof context, safe
experiments, verification, semantic lookup, and cheap status without exposing
raw Lean worker primitives.

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

Every call is read-only. The server reads files, elaborates in memory, and never
writes source.

## Tool Roles

- `lean_status`: cheap project, toolchain, build-artifact, and diagnostics
  status. Use it before spending a worker permit or when you need file
  diagnostics.
- `lean_context`: local proof state at a declaration position. Use it to see
  goals, locals, expected type, and diagnostics before choosing a tactic.
- `lean_trial`: non-mutating probes. Use `proof_step` to try tactics and
  `command` for `#check`, `#eval`, or `#print axioms`.
- `lean_lookup`: semantic discovery. Use it for declarations, declaration
  inventory, proof search, reference search, and changed-declaration coverage.
- `lean_verify`: verification gates. Use it when a declaration or changed set
  must be checked with `sorry` policy and optional axiom reporting.

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

## Output Carrier

By default, the serialized semantic response is placed in MCP `content` text so
model clients can read it reliably. Programmatic clients can request structured
output at server startup:

```sh
LEAN_HOST_MCP_RESPONSE_CARRIER=structured lean-host-mcp --lake-root /path/to/project --bind 127.0.0.1:8765
```

Use `LEAN_HOST_MCP_RESPONSE_CARRIER=both` to mirror the same JSON into both
`content` and `structuredContent`.

## Common Workflows

### Inspect Project Status

```json
{
  "name": "lean_status",
  "arguments": {
    "kind": "project",
    "include": ["toolchain", "worker", "artifacts"]
  }
}
```

When artifact freshness is `unknown`, `lean_status` has only checked cheap
filesystem facts. Run `lean_lookup(kind="references")`,
`lean_lookup(kind="declarations")`, or `lake build` to establish freshness for a
specific semantic task.

### Get Diagnostics For A File

```json
{
  "name": "lean_status",
  "arguments": {
    "kind": "file_diagnostics",
    "file": "LeanRsFixture/ProofActions.lean"
  }
}
```

### Query Declarations By Name Or Inventory

Inspect one known declaration:

```json
{
  "name": "lean_lookup",
  "arguments": {
    "kind": "declaration",
    "name": "Nat.add_zero",
    "imports": ["Init"]
  }
}
```

List declarations in a module:

```json
{
  "name": "lean_lookup",
  "arguments": {
    "kind": "declarations",
    "target": { "kind": "module", "module": "LeanRsFixture.ProofAgent" },
    "limit": 200
  }
}
```

List declarations in a file:

```json
{
  "name": "lean_lookup",
  "arguments": {
    "kind": "declarations",
    "target": { "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" },
    "limit": 200
  }
}
```

For prefix-style browsing, use the file or module inventory call and filter the
returned declaration names on the client.

### Inspect Proof State At A Position

```json
{
  "name": "lean_context",
  "arguments": {
    "kind": "proof_position",
    "file": "LeanRsFixture/ProofActions.lean",
    "declaration": "LeanRsFixture.ProofActions.stepTheorem",
    "proof_position": { "kind": "default" }
  }
}
```

### Verify One Declaration With Axiom Reporting

The old user-facing phrase `verify_declaration` maps to `lean_verify` with one
explicit target group:

```json
{
  "name": "lean_verify",
  "arguments": {
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
}
```

### Verify Changed Declarations

```json
{
  "name": "lean_verify",
  "arguments": {
    "targets": [
      {
        "kind": "changed",
        "base": "HEAD",
        "files": ["LeanRsFixture/ProofActions.lean"],
        "include_untracked": true
      }
    ],
    "allow_sorry": false,
    "report_axioms": true
  }
}
```

## Proof Workflow

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

### Tagged Request Shapes

Several arguments are tagged enums. The tag is always a string field named
`kind`.

`DeclarationInventoryTarget` for `lean_lookup(kind="declarations")`:

```json
{ "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" }
```

or:

```json
{ "kind": "module", "module": "LeanRsFixture.ProofAgent" }
```

`ProofPositionSelector` for `lean_context(kind="proof_position")`,
`lean_trial(kind="proof_step")`, and `lean_lookup(kind="proof_search")`:

```json
{ "kind": "default" }
```

```json
{ "kind": "index", "index": 0 }
```

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

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

for a worker-recognized proof-state boundary matching a source fragment. Not
every substring is a boundary; inspect the returned `goals_before` and
`goals_after` to determine the exact state available at the match.

If an `after_text` selector does not resolve, the result stays a normal
Lean-domain response and includes valid `proof_boundaries`:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "after_text", "text": "not a boundary" }
}
```

```json
{
  "status": "context",
  "unavailable": [
    {
      "id": "proof_state",
      "message": "declaration has no proof position matching the selector"
    }
  ],
  "proof_boundaries": [
    {
      "index": 0,
      "kind": "entry",
      "selector": { "kind": "default" },
      "source": { "start_line": 2, "start_column": 3, "end_line": 2, "end_column": 10 },
      "excerpt": { "value": "intro h", "truncated": false }
    },
    {
      "index": 1,
      "kind": "after_tactic",
      "selector": { "kind": "index", "index": 1 },
      "source": { "start_line": 3, "start_column": 3, "end_line": 3, "end_column": 10 },
      "excerpt": { "value": "exact h", "truncated": false }
    }
  ],
  "proof_boundaries_truncated": false
}
```

Retry with the returned selector:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "index", "index": 1 }
}
```

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

Proof-step diagnostics label their coordinate space. Candidate-local diagnostics
usually point into the synthetic trial buffer, so use `synthetic_range` for
display and do not treat it as an editable file range unless `original_range` is
also present:

```json
{
  "id": "candidate_1",
  "status": "failed",
  "diagnostics": {
    "diagnostics": [
      {
        "severity": "error",
        "message": "unknown identifier 'definitely_missing_identifier'",
        "coordinate_space": "synthetic_buffer",
        "position": { "line": 82, "column": 9, "end_line": 82, "end_column": 39 },
        "synthetic_range": { "line": 82, "column": 9, "end_line": 82, "end_column": 39 }
      }
    ],
    "truncated": false
  }
}
```

### `kind: "command"`

Runs bounded Lean command text as a non-mutating trial. Use it for import-context
snippets such as `#check` and `#print axioms`; it is not a replacement for
project-wide shell workflows.

Explicit imports:

```json
{
  "kind": "command",
  "imports": ["Init"],
  "commands": "#check Nat.add\n#print axioms Nat.add_assoc"
}
```

File-derived context prepends the current source snapshot before the command
text, so declarations in that file are visible to later commands:

```json
{
  "kind": "command",
  "file": "LeanRsFixture/ProofActions.lean",
  "commands": "#check LeanRsFixture.ProofActions.closedTheorem"
}
```

Info-level command messages are collected into `output.value`; errors and
warnings remain in the bounded diagnostics block. Invalid command snippets are
normal results with diagnostics, not MCP transport failures.

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
    { "kind": "module_all", "module": "LeanRsFixture.ProofActions" },
    {
      "kind": "changed",
      "base": "HEAD",
      "files": ["LeanRsFixture/ProofActions.lean"],
      "include_untracked": true
    }
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
    "unknown_coverage": 1,
    "truncated": false
  },
  "results": [
    {
      "id": "group_1:LeanRsFixture.ProofActions.closedTheorem",
      "file": "LeanRsFixture/ProofActions.lean",
      "declaration": "LeanRsFixture.ProofActions.closedTheorem",
      "reason": "hunk_overlaps_body",
      "verification_status": "verified",
      "facts": {}
    }
  ],
  "coverage": {
    "unknown": [
      {
        "file": "LeanRsFixture/ProofActions.lean",
        "reason": "hunk_outside_declaration",
        "next_action": "verify the whole file or run lake build and retry"
      }
    ],
    "deleted_files": [],
    "renamed_files": [],
    "truncated": false
  }
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

`changed` runs non-interactive git commands under the project root:
`git diff --unified=0 --no-ext-diff --find-renames <base> -- '*.lean'`, plus
`git ls-files --others --exclude-standard -- '*.lean'` when
`include_untracked` is true. It maps changed hunks to source-fresh declaration
spans and verifies only known declarations. Coverage is conservative: comment
or whitespace hunks outside any declaration, unavailable/truncated declaration
inventory, deleted files, and renames are reported under `coverage` instead of
being silently dropped. If coverage is unknown, verify the whole file or rebuild
and retry; the server will not trust stale `.ilean` rows as authoritative for
editable changed source.

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

### `kind: "changed_coverage"`

Reports how git hunks map to source-fresh declarations without verifying them.
The request fields match the `lean_verify` changed target:

```json
{
  "kind": "changed_coverage",
  "base": "HEAD",
  "files": ["LeanRsFixture/ProofActions.lean"],
  "include_untracked": true
}
```

The result has `known` changed declarations and the same `coverage` block that
`lean_verify` returns. Unknown rows are actionable coverage gaps, not failures
to be ignored.

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

### `kind: "file_diagnostics"`

Elaborates the current source snapshot and returns the same bounded diagnostics
block shape used by proof context. This surfaces Lean diagnostics and linter
messages produced while elaborating the file; it does not run `lake build`,
`lake exe lint`, or other external project-specific lint commands.

```json
{
  "kind": "file_diagnostics",
  "file": "LeanRsFixture/ProofActions.lean"
}
```

The result includes `diagnostics` and the header `imports` used for the worker
session, with a source `edit_fresh` trust fact for the file snapshot.

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
