# Tool Catalog

`lean-host-mcp` exposes five semantic Lean tools. Four tools use a `kind` field to select a mode inside that job family:
`lean_context`, `lean_trial`, `lean_lookup`, and `lean_status`. `lean_verify` takes target groups directly. The surface
is intentionally small: it gives an agent proof context, safe experiments, verification, semantic lookup, and cheap
status without exposing raw Lean worker primitives.

```text
lean_context -> lean_lookup -> lean_trial -> lean_verify
                         \-> lean_status
```

Every call is read-only. The server reads files, elaborates in memory, and never writes source.

## Tool Roles

- `lean_status`: cheap project, toolchain, build-artifact, and diagnostics status. Use it before spending a worker
  permit or when you need file diagnostics.
- `lean_context`: local proof state at a declaration position. Use it to see goals, locals, expected type, and
  diagnostics before choosing a tactic.
- `lean_trial`: non-mutating probes. Use `proof_step` to try tactics and `command` for `#check`, `#eval`, or
  `#print axioms`.
- `lean_lookup`: semantic discovery. Use it for declarations, declaration inventory, proof search, and reference search.
- `lean_verify`: verification gates. Use it when a declaration, file, or module must be checked with `sorry` policy and
  optional axiom reporting.

When an MCP request carries `_meta.progressToken`, every tool emits a start notification, a heartbeat every 15 seconds,
and a completion notification on `notifications/progress`. Clients such as Claude Code extend their per-call deadline
only while progress arrives, so a cold import or a large file's elaboration is not abandoned at the client while the
worker keeps computing. Clients that omit the token keep the same single-response behavior.

## Response Shape

All public tools return the semantic response baseline:

```jsonc
{
  "data": { "...": "mode-specific result" },
  "errors": [],
  "trust": {
    "project_root": "/abs/project",
    "session_id": "metadata-only-or-worker-session",
    "lean_toolchain": "leanprover/lean4:v4.34.0-rc2",
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

Rows may also carry `path`, `module`, `detail`, and `next_action`. Paths are rendered project-relative when the file is
inside the resolved Lake root, and absolute only for files outside that root. Source-overlay tools and source-backed
declaration inventory report the source file snapshot as `source` / `file` / `edit_fresh`; project reference lookup and
index-backed declaration inventory report `.ilean` build freshness or missing/stale build state; `needs_build`
degradations report missing `.olean` artifacts. Duplicate trust rows are collapsed while preserving first occurrence
order. Runtime counters, cache timings, and import lists remain telemetry, not trust.

Lean-domain outcomes remain data. A failed tactic, a rejected declaration, an ambiguous name, or a `needs_build` verdict
is not an MCP transport error. Infrastructure failures that the client can retry, such as project mailbox pressure or
restart-loop exhaustion, appear in `errors` with structured details. Warnings and next actions from the underlying
implementation are also carried as warning issues in `errors`.

### Call timing and the slow-call warning

Every call's telemetry `runtime` block carries three timing facts: `queue_wait_millis` (admission delay before the
worker started the call), `call_elapsed_millis` (worker-call wallclock, queue wait excluded, including any recycle and
retries the call triggered), and `call_cpu_millis` (the worker child process's own CPU delta around the call — sampled
at 10 ms granularity on Linux, nanosecond precision on macOS — omitted
when the platform cannot sample it or a mid-call recycle invalidated the baseline). Under the default
`telemetry.verbosity = quiet` the telemetry block is dropped, but the actionable signal is not: when
`call_cpu_millis` exceeds `output.slow_call_warning_millis` (default 10000), the response carries a warning issue
naming the CPU figure and recommending refactoring (split the declaration, add explicit type annotations, extract
intermediate lemmas). Keying on CPU rather than wallclock keeps machine contention from ever triggering it; when
wallclock is more than twice the CPU figure the warning says so instead.

Omission convention: completeness flags named `*_truncated` (plus `RenderedText.truncated`, `partial`, and advisory
containers such as `post_closure_diagnostics`, `entry_goals`, and `locals`) are omitted from the JSON when they carry
their empty/default value (`false` or empty); an absent flag means complete/untruncated. The examples below show
`true`/populated cases where relevant and may omit the default-valued keys. Verdict and trust facts — `contains_sorry`,
`contains_admit`, `contains_sorry_ax`, `axioms_available`, `facts_trustworthy`, and declaration attribute flags — always
serialize, `false` included, so a client never silently defaults a check that matters.

## Output Carrier

By default, the serialized semantic response is placed in MCP `content` text so model clients can read it reliably.
Programmatic clients can request structured output at server startup:

```sh
LEAN_HOST_MCP_RESPONSE_CARRIER=structured lean-host-mcp --lake-root /path/to/project --bind 127.0.0.1:8765
```

Use `LEAN_HOST_MCP_RESPONSE_CARRIER=both` to mirror the same JSON into both `content` and `structuredContent`.

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

When artifact freshness is `unknown`, `lean_status` has only checked cheap filesystem facts. Run
`lean_lookup(kind="references")`, `lean_lookup(kind="declarations")`, or `lake build` to establish freshness for a
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

For prefix-style browsing, use the file or module inventory call and filter the returned declaration names on the
client.

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

The old user-facing phrase `verify_declaration` maps to `lean_verify` with one explicit target group:

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

### Request Synonyms

Requests are decoded forgivingly. The recurring synonyms agents write are mapped onto the canonical schema before typed
decoding, so the first call succeeds; the canonical shapes below remain the documented ones, and responses never use
the synonyms.

- Modes: `lean_lookup` `signature`/`print`/`decl` → `declaration`; `inventory`/`outline` → `declarations`; `lean_trial`
  `tactic`/`step` → `proof_step`, `snippet`/`eval`/`check` → `command`; `lean_context` `goal`/`state` →
  `proof_position`; `lean_status` `health`/`capabilities` → `project`, `diagnostics` → `file_diagnostics`.
- Fields: `lean_lookup(kind="declarations")` accepts a bare `file`/`path` or `module` in place of `target`;
  `lean_lookup(kind="declaration")` accepts `declaration` for `name` and `value`/`type`/`docs` as field names;
  `lean_trial(kind="command")` accepts `command`, `code`, or a line array for `commands`; `lean_trial(kind="proof_step")`
  accepts `tactic` for `snippet` and `candidates` for `snippets`; a `proof_position` given as a string is the entry goal
  (`"start"`) or an `after_text` match, and as an integer an `index`.
- `lean_verify` accepts one group at top level (`file` with `declarations`, `file` alone, or `module` alone), a singular
  `target`, string groups (`"Proofs/A.lean"`, `"Proofs.A"`), and the group kinds `file` → `file_all`, `module` →
  `module_all`, `declarations` → `explicit`.
- A `kind` that names another tool's job (`search`, `file`, `verify`, …) is still rejected, with the error naming the
  tool and mode to call instead.

## Proof Workflow

1. Call `lean_context` with `kind: "proof_position"` to read the current proof goals, locals, and diagnostics for a
   declaration position. Add `include_boundaries: true` when you need the boundary list to pick a selector, and
   `include_expected_type: true` when you need the goal's expected type; both default to `false`.
2. Call `lean_lookup` with `kind: "proof_search"` to retrieve ranked declarations for the goal.
3. Call `lean_lookup` with `kind: "declaration"` to inspect a promising declaration's statement, docstring, attributes,
   and flags.
4. Call `lean_trial` with `kind: "proof_step"` to try one or more tactics in memory without editing the file. The trial
   envelope is self-contained: it carries `entry_goals` and `locals` for the resolved position, so iterating proof steps
   does not require a `lean_context` round-trip before each trial.
5. Call `lean_verify` with an explicit target group to verify the target declaration.

Use `lean_lookup` with `kind: "references"` when the task is semantic reference discovery rather than proof search. Use
`lean_status` for cheap project and host status before spending a worker permit.

### Tagged Request Shapes

Several arguments are tagged enums. The tag is always a string field named `kind`.

`DeclarationInventoryTarget` for `lean_lookup(kind="declarations")`:

```json
{ "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" }
```

or:

```json
{ "kind": "module", "module": "LeanRsFixture.ProofAgent" }
```

`ProofPositionSelector` for `lean_context(kind="proof_position")`, `lean_trial(kind="proof_step")`, and
`lean_lookup(kind="proof_search")`:

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

Returns proof context for one declaration proof position. The request fields are the existing declaration anchor plus an
optional proof-position selector:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "default" }
}
```

**Breaking change (context trim).** The response no longer echoes `declaration_name` or `namespace_name` — the caller
already named the declaration in the request. Two heavier payload fields are now opt-in flags on the request, both
defaulting to `false`:

- `include_boundaries: true` returns the `proof_boundaries` navigation list (and `proof_boundaries_truncated`). Request
  it once per declaration to pick a position selector.
- `include_expected_type: true` returns the goal's `expected_type`.

The intended loop: call `lean_context` with `include_boundaries: true` once per declaration to navigate, then drive
every step with self-contained `lean_trial(kind="proof_step")` calls (which carry their own `entry_goals` and `locals`),
instead of re-reading context at each step.

When `proof_position` is omitted, the default is the pristine entry goal: the state before any tactic runs. This is the
same position where a default `lean_trial(kind="proof_step")` snippet is spliced.

Other selectors are:

```json
{ "kind": "index", "index": 0 }
```

for the state after the first tactic, and:

```json
{ "kind": "after_text", "text": "skip", "occurrence": 0 }
```

for a worker-recognized proof-state boundary matching a source fragment. Not every substring is a boundary; inspect the
returned `goals_before` and `goals_after` to determine the exact state available at the match.

If an `after_text` selector does not resolve, the result stays a normal Lean-domain response and — when
`include_boundaries` is set — includes valid `proof_boundaries`:

```json
{
  "kind": "proof_position",
  "file": "LeanRsFixture/ProofActions.lean",
  "declaration": "LeanRsFixture.ProofActions.stepTheorem",
  "proof_position": { "kind": "after_text", "text": "not a boundary" },
  "include_boundaries": true
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
      "excerpt": { "value": "intro h" }
    },
    {
      "index": 1,
      "kind": "after_tactic",
      "selector": { "kind": "index", "index": 1 },
      "source": { "start_line": 3, "start_column": 3, "end_line": 3, "end_column": 10 },
      "excerpt": { "value": "exact h" }
    }
  ]
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

The `data` payload is the proof context result previously produced internally by the proof-position operation: status,
diagnostics, goals, locals, expected type, truncation, and any `needs_build` or ambiguity facts.

## `lean_trial`

### `kind: "proof_step"`

Tries one or more proof snippets at a declaration proof position against an in-memory source snapshot. It never writes
files.

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

The worker attempts candidates in request order and returns at most 16 ordered rows. `candidate_limit` records that cap,
`candidates_truncated` is true when more candidates were requested than could be represented, and `summary` gives the
batch counts:

```json
{
  "candidates": [
    { "id": "candidate_1", "status": "closed" },
    { "id": "candidate_2", "status": "failed" }
  ],
  "candidate_limit": 16,
  "summary": {
    "requested_candidates": 2,
    "returned_candidates": 2,
    "candidate_limit": 16,
    "closed": 1,
    "progressed": 0,
    "failed": 1,
    "timeout": 0,
    "budget_exceeded": 0,
    "not_attempted": 0,
    "unsupported": 0,
    "output_truncated": 0
  }
}
```

Candidate status is one of `closed`, `progressed`, `failed`, `timeout`, `budget_exceeded`, `not_attempted`, or
`unsupported`. A batch is partial when some rows timed out, exceeded the total output budget, were skipped after that
budget was exhausted, or had truncated output. In that case the envelope also includes warnings/next actions suggesting
a smaller batch, a single-candidate retry, or a larger `output.max_total_bytes` budget.

The envelope also carries `entry_goals` and `locals`: the goal state and local hypotheses at the resolved proof position
before any candidate was spliced — the same values `lean_context(kind="proof_position")` reports as `goals_before` and
`locals` at that position. They are rendered once per batch and shared by every candidate row, and both are omitted from
the JSON when empty (a degraded or unresolvable entry state yields empty arrays, never an error). A trial loop therefore
no longer needs a `lean_context` call before each step: read the context once to navigate, then drive subsequent steps
from the trial envelopes alone.

`retry_tainted_non_positive` (default `false`) opts into one server-side retry: when the worker was recycled mid-call
and the batch is non-positive (no candidate `closed` or `progressed`), the server re-issues the attempt once and returns
the retry's rows instead of the suspect ones. If the retry is also tainted and non-positive, the response keeps the
usual execution-taint warning and the retry decision returns to the client. At most one retry per call; it is surfaced
through `runtime.retry_count`. With the flag unset the behavior is unchanged: the server reports the taint and the
client decides.

Proof-step diagnostics label their coordinate space. Candidate-local diagnostics usually point into the synthetic trial
buffer, so use `synthetic_range` for display and do not treat it as an editable file range unless `original_range` is
also present:

Across all candidate statuses, the first error-severity diagnostic in `diagnostics` is a reliable failure signal: a
`closed` candidate's `diagnostics` and `downstream_diagnostics` carry only non-error advisory notes (warnings and info).
Error-severity entries a closed candidate produced after closing its goal — for example the original downstream tactics
reporting "no goals" — are moved, never deleted, into the advisory `post_closure_diagnostics` field, which is omitted
from the JSON when empty and appears only on `closed` candidates.

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
    ]
  }
}
```

### `kind: "command"`

Runs bounded Lean command text as a non-mutating trial. Use it for import-context snippets such as `#check` and
`#print axioms`; it is not a replacement for project-wide shell workflows.

Explicit imports:

```json
{
  "kind": "command",
  "imports": ["Init"],
  "commands": "#check Nat.add\n#print axioms Nat.add_assoc"
}
```

File-derived context prepends the current source snapshot before the command text, so declarations in that file are
visible to later commands:

```json
{
  "kind": "command",
  "file": "LeanRsFixture/ProofActions.lean",
  "commands": "#check LeanRsFixture.ProofActions.closedTheorem"
}
```

Info-level command messages are collected into `output.value`; errors and warnings remain in the bounded diagnostics
block. Invalid command snippets are normal results with diagnostics, not MCP transport failures.

## `lean_verify`

Verifies declarations in memory. Targets can be explicit declaration lists, every declaration in a file, or every
declaration in a module. The server reads Lean source and calls Lean's elaborator/kernel through the worker; it does not
run `lake build`.

By-name targets resolve every surface declaration form the elaborator knows: multi-clause equation `def`s and theorems,
`where`-structure defs, `structure`/`class` commands, and anonymous `instance`s (under their generated `inst…` names —
discover them via `lean_lookup(kind="declarations")`). A `not_found` verdict means the name is genuinely absent from the
file, never that the declaration uses one of these forms.

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
  "report_axioms": true,
  "detail": "compact"
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
  "report_axioms": false,
  "detail": "compact"
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
    "unknown_coverage": 1
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
    "renamed_files": []
  }
}
```

`verification_status` uses the same vocabulary as the declaration-verification projection: `verified`, `has_sorry`,
`has_unresolved_goals`, `has_diagnostics`, `not_found`, `ambiguous`, `needs_build`, `timeout`, `budget_exceeded`,
`worker_recycled`, or `unsupported`. `requested` counts expanded targets before host-side caps; `truncated` is true when
declaration inventory or verification output was capped. `file_all` and source-backed `module_all` use the current
source snapshot and report the same project-relative file paths in rows. If a module has no source file, `module_all`
may use the `.ilean` declaration inventory, with typed artifact freshness facts in `trust`. Every source-backed trust
fact includes `content_sha256`, the SHA-256 of the exact bytes used by the call.

`detail` defaults to `"compact"`, which keeps the row-level declaration name, file, status, reason, diagnostics,
sorry/admit facts, axiom facts, and trustworthiness flags, but omits repeated target-span and ambiguous-candidate span
blocks. Use `"detail": "full"` when you need those source spans in each row.

`retry_tainted_non_positive` (default `false`) opts into one server-side retry per target-group batch: when the worker
was recycled mid-call and the batch comes back non-positive (any row that would be relabeled `worker_recycled`), the
server re-issues that batch once. `verified` rows are never retried — verification is monotone. If the retry is also
tainted and non-positive, the rows are relabeled to `worker_recycled` with the usual warning, exactly as with the flag
unset. At most one retry per batch; it is surfaced through `runtime.retry_count`.

A `not_found` row for an explicit target carries, in `facts.candidates` (with `"detail": "full"`) and in a response
warning, the file's declarations whose short name matches the requested one across namespaces and case — the usual
cause is a namespace or capitalization slip, and the warning names the intended declaration so the next call can use
it. Mapping a git diff to declarations is the caller's job; the stacks verifier does it from `git diff` and passes
explicit groups.

## `lean_lookup`

### `kind: "declaration"`

Inspects one declaration by name. Use `file` when local imports or namespace context are needed; use `imports` for
explicit import context.

```json
{
  "kind": "declaration",
  "name": "Nat.add_zero",
  "imports": ["Init"]
}
```

Optional `fields` can select `source`, `statement`, `docstring`, `attributes`, and `flags`; `raw_statement` asks for the
raw elaborated term.

### `kind: "declarations"`

Lists declarations in one source file or module. This is declaration inventory, so it is a semantic lookup mode rather
than a separate public tool.

File targets read the current source snapshot and use the worker declaration outline selector:

```json
{
  "kind": "declarations",
  "target": { "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" },
  "limit": 200
}
```

Module targets first resolve `<module>.lean` under the project root and use the same source-fresh path when the file
exists:

```json
{
  "kind": "declarations",
  "target": { "kind": "module", "module": "LeanRsFixture.ProofAgent" },
  "limit": 200
}
```

If a module source file is unavailable but a matching `.ilean` exists, the mode returns build-fresh index rows instead.
Index rows know the declaration range and name/selection range but not the declaration kind or body span, so `kind` and
`body_span` are omitted. If neither source nor index is available, the result status is `missing_build` or `not_found`,
never an empty successful list. `limit` defaults to 200 and is capped at 1000; truncation keeps a deterministic prefix
and sets `truncated: true`.

### `kind: "proof_search"`

Returns ranked declarations relevant to a proof goal. The target can come from a file/declaration position:

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

Modes are `next_step`, `exact`, `apply`, `rewrite`, and `simp`. `limit` is clamped to the tool cap.

### `kind: "references"`

Finds semantic references to a fully-qualified Lean name.

File scope elaborates one anchor file through the worker, so it reflects the current source snapshot and carries a
worker session id plus a `source` / `file` / `edit_fresh` trust fact:

```json
{
  "kind": "references",
  "name": "LeanRsFixture.ProofActions.closedTheorem",
  "scope": "file",
  "file": "LeanRsFixture/ProofActions.lean",
  "limit": 20
}
```

Project scope reads the on-disk `.ilean` reference index and does not open a worker:

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

Returns cheap project, toolchain, output, and broker configuration. This mode uses Lake metadata only and does not open
a worker.

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

Elaborates the current source snapshot and returns the same bounded diagnostics block shape used by proof context. This
surfaces Lean diagnostics and linter messages produced while elaborating the file; it does not run `lake build`,
`lake exe lint`, or other external project-specific lint commands.

```json
{
  "kind": "file_diagnostics",
  "file": "LeanRsFixture/ProofActions.lean"
}
```

The result includes `diagnostics` and the header `imports` used for the worker session, with a source `edit_fresh` trust
fact for the file snapshot.

## Maintainer Migration Table

The old public tools are no longer registered. Existing implementation code is still reused internally behind the
semantic modes.

| Old public tool | New public tool and mode |
| --- | --- |
| `proof_state` | `lean_context`, `kind: "proof_position"` |
| `try_proof_step` | `lean_trial`, `kind: "proof_step"` |
| `verify_declaration` | `lean_verify` with one `kind: "explicit"` target group |
| `inspect_declaration` | `lean_lookup`, `kind: "declaration"` |
| `search_for_proof` | `lean_lookup`, `kind: "proof_search"` |
| `find_references` | `lean_lookup`, `kind: "references"` |

Do not re-add compatibility aliases for the old names.
