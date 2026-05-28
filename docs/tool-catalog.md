# Tool catalogue

Every tool returns the same outer envelope (see the README). Only `result` differs; that's what this document records.

The envelope's `freshness.session_id` is the **stable identity of the project actor that served the call**: two
responses with the same `(project_root, session_id)` pair were served by the same in-process worker. `session_id` only
changes when the broker re-spawns a project: LRU eviction (pool full, project not used recently), idle eviction
(`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`), or manifest invalidation (`lake-manifest.json` changed on disk). Clients can detect
"my project was restarted between these two calls" by comparing `session_id` alone.

## Per-tool routing field

Every request schema accepts an optional `project` field: an absolute path to a Lake-project root. When set, the server
routes that single call to that project; the broker keeps up to `LEAN_HOST_MCP_MAX_PROJECTS` (default 4) projects
resident concurrently, so per-call routing does not cost a re-spawn unless the pool is full and the requested project
was recently evicted. When omitted, the server resolves the project via the standard chain *env
(`LEAN_HOST_MCP_PROJECT`) → cwd-walk → `~/.config/lean-host-mcp/config.toml` `primary_project`*.

```jsonc
// Any tool request, routed to a non-default project:
{ "source": "(1 : Nat)", "imports": [], "project": "/abs/path/to/other/lake/root" }
```

The `project` field is omitted from the per-tool examples below for brevity; assume it on every request schema.

Lean session and declaration tools also accept an `imports` field. It is the complete per-call import vector. An empty
array means the call asks for no extra imports beyond the worker's base environment.

## Lean session tools (`src/tools/lean.rs`)

### `elaborate`

Type-check a term against the project environment.

```jsonc
// request
{ "source": "(Nat.succ 0 : Nat)", "imports": ["Mod.A"] }

// result
{ "status": "Ok",     "ok": true }
{ "status": "Failed", "diagnostics": [...], "truncated": false }
```

### `kernel_check`

Full elaborate plus `addDecl`. Returns Lean's `LeanKernelOutcome`, projected.

```jsonc
// request
{ "source": "theorem foo : 1 + 1 = 2 := rfl", "imports": [] }

// result
{
  "status":  "Checked",
  "summary": { "declaration_name": "foo", "kind": "theorem", "type_signature": "1 + 1 = 2" },
  "failure": null
}
```

### `infer_type` / `whnf`

`Meta.inferType` or `Meta.whnf` over a term that is elaborated first.

```jsonc
{ "term": "Nat.succ 0", "imports": [] }
```

`rendered` is pretty-printed via the optional `pp_expr` shim. If the loaded worker host shims lack
`lean_rs_host_meta_pp_expr`, the worker falls back to `Expr.toString` (signalled by `LeanWorkerRendering::Raw`) and the
server attaches a warning to the envelope; the field is still populated either way.

### `is_def_eq`

```jsonc
// request
{ "lhs": "1 + 1", "rhs": "2", "imports": [], "transparency": "reducible" }

// result
{ "status": "Ok", "definitionally_equal": true, "rendered": null, "failure": null }
```

`transparency` is optional and accepts `default | reducible | instances | all` (default `default`). It picks the
reducibility view `Meta.isDefEq` runs under: the same two terms can be def-eq under one setting and not another.

### `inspect_declaration`

```jsonc
// request
{ "name": "Nat.add_zero", "imports": [], "max_field_bytes": 8192, "max_total_bytes": 65536 }

// result
{
  "status": "found",
  "declaration": {
    "name": "Nat.add_zero",
    "kind": "theorem",
    "module": "Init.Prelude",
    "source": { ... },
    "statement": { "value": "∀ (n : Nat), n + 0 = n", "truncated": false },
    "docstring": null,
    "attributes": ["simp"],
    "proof_search": {
      "is_simp": true,
      "is_rw_candidate": true,
      "is_instance": false,
      "is_class": false,
      "class_name": null
    },
    "flags": { "is_private": false, "is_generated": false, "is_internal": false }
  }
}
{ "status": "not_found", "name": "Nat.foo_bar" }
```

Cursor form resolves the declaration target first, then inspects that one declaration:

```jsonc
{ "file": "LeanRsFixture/SourceRanges.lean", "line": 8, "column": 3 }
```

`fields` can disable `source`, `statement`, `docstring`, `attributes`, or `flags`; omitted field switches default to
enabled. Rendered `statement` and `docstring` always carry `truncated`. `max_field_bytes` defaults to 8192 and clamps to
65536; `max_total_bytes` defaults to 65536. Cursor resolution can also return `ambiguous` or `unsupported`.

Use `search_for_proof` for proof-oriented retrieval. It returns bounded candidate metadata; inspect one selected
candidate by name when statement text or declaration facts are needed.

### `try_proof_step`

Try one or more proof snippets against a file snapshot. The tool reads the file, resolves a safe proof edit at the
cursor, sends an in-memory overlay to Lean, and never writes the source file.

```jsonc
// request
{
  "file": "LeanRsFixture/ProofActions.lean",
  "line": 4,
  "column": 3,
  "snippet": "trivial"
}

// result
{
  "status": "ok",
  "result": {
    "candidate_limit": 8,
    "candidates_truncated": false,
    "candidates": [
      {
        "id": "candidate_1",
        "status": "closed",
        "diagnostics": { "diagnostics": [], "truncated": false },
        "goals": [],
        "safe_edit": { "declaration_name": "LeanRsFixture.ProofActions.closedTheorem", "...": "..." },
        "output_truncated": false
      }
    ]
  },
  "imports": []
}
```

`snippet` is a convenience for one candidate. `snippets` accepts a small list; the host sends at most 8 candidates to
Lean and returns extra rows with `status: "budget_exceeded"`. Candidate statuses are `closed`, `progressed`, `failed`,
`timeout`, `budget_exceeded`, or `unsupported`. Bad snippets are normal rows, not MCP errors. `mode` defaults to
`safe_edit`; `insert_at` and `declaration_body` are available for narrower edit-target requests.

### `verify_declaration`

Verify one declaration in a file snapshot under a sorry/axiom policy. The tool never writes the source file.

```jsonc
// request
{
  "file": "LeanRsFixture/ProofActions.lean",
  "name": "LeanRsFixture.ProofActions.closedTheorem",
  "allow_sorry": false,
  "report_axioms": true
}

// result
{
  "status": "ok",
  "verification_status": "verified",
  "facts": {
    "target": { "declaration_name": "LeanRsFixture.ProofActions.closedTheorem", "...": "..." },
    "diagnostics": { "diagnostics": [], "truncated": false },
    "unresolved_goals": [],
    "contains_sorry": false,
    "contains_admit": false,
    "contains_sorry_ax": false,
    "axioms": [],
    "axioms_truncated": false,
    "output_truncated": false
  },
  "imports": []
}
```

The target can be a declaration `name` or a cursor `line`/`column`. `verification_status` is one of `verified`,
`has_diagnostics`, `has_sorry`, `has_unresolved_goals`, `not_found`, `ambiguous`, `timeout`, `budget_exceeded`, or
`unsupported`. Policy failures are data in the result, so a theorem containing `sorry` returns a successful MCP response
with `verification_status: "has_sorry"` when `allow_sorry` is false.

## Source search (`src/tools/scan.rs`)

### `source_search`

Bounded source/text search over the project's `.lean` files. No Lean dependency; results are textual matches, not
elaborator output or semantic claims. Its freshness envelope reports `imports: []`.

```jsonc
{ "preset": "sorry" }
{ "preset": "custom", "pattern": "@\\[simp\\]", "limit": 100 }
```

Presets: `sorry | admit | axiom | imports | namespaces | declaration_names | theorem_statements | custom`.

Results include `matches`, `files_scanned`, `files_skipped`, `truncated`, and `source_based: true`.

## Mathlib placement (`src/tools/placement.rs`)

### `mathlib_placement`

Bounded host-policy advice for where a declaration belongs in a Mathlib-compatible source layout. It scans Mathlib
source text under an explicit or project-discovered root and may sample `inspect_declaration` / `search_for_proof` for
the one selected declaration or statement. It never performs all-Mathlib semantic elaboration.

```jsonc
// request with an explicit Mathlib source root
{
  "statement": "theorem map_append_new : True",
  "concepts": ["List", "map_append"],
  "proposed_name": "map_append_new",
  "mathlib_root": "/abs/path/to/project/.lake/packages/mathlib/Mathlib"
}
```

When `mathlib_root` is omitted, discovery is limited to the routed project: `<project>/Mathlib` and
`<project>/.lake/packages/mathlib/Mathlib`. Missing roots return `status: "missing_mathlib_root"` with the checked
paths; there is no user-machine fallback path.

Placement results include likely namespace/file/imports, nearby source declarations, possible duplicates, naming
examples, upstream-readiness notes, source facts, semantic facts, and warnings. Source facts are explicitly
source-based; semantic facts come only from selected bounded worker calls.

## Proof-agent module tools (`src/tools/position.rs`)

`proof_state` and `lean_query` drive `process_module_query_batch`: they read one file, hand the full source (header +
body) to Lean's frontend, and ask for bounded semantic projections. `proof_state` is the normal proof-agent entry point
and returns one compact context object under a 64 KiB default batch cap. `lean_query` is the expert batch form for
callers that need to choose selectors directly. The file's own `import` declarations are parsed by Lean and validated
against the server's open env; mismatch surfaces as an envelope `warnings` entry. Query results include `query_facts`,
the worker-reported cache/timing record for the module snapshot used by the batch. The host passes the canonical file
path as Lean's file label but does not keep an exact batch-result cache for these tools; identical repeated calls reach
the worker so cache hits, rebuilds, and evictions remain visible.

`process_module_query_batch` is an **optional** capability shim. When the loaded dylib lacks it, these tools answer
`{ "status": "unsupported" }` cleanly; the tools never raise and never request a whole-file info tree.

All line and column inputs are **1-indexed**. Result spans use the same convention.

### `proof_state`

Inspect the current Lean proof context at a cursor position. This is the default tool before editing a proof.

```jsonc
// request
{ "file": "LeanRsFixture/SourceRanges.lean", "line": 8, "column": 3 }

// result
{
  "status": "context",
  "diagnostics": { "summary": { "errors": 0, "warnings": 0, "info": 0 }, "diagnostics": [], "truncated": false },
  "declaration_name": "LeanRsFixture.SourceRanges.proofGoal",
  "namespace_name": "LeanRsFixture.SourceRanges",
  "safe_edit": { "declaration_name": "...", "body_span": { ... } },
  "span": { "start_line": 8, "start_column": 3, "end_line": 8, "end_column": 10 },
  "goals_before": ["⊢ True"],
  "goals_after": [],
  "locals": [],
  "term": { "status": "term", "type_str": { "value": "TacticM Unit", "truncated": false }, "...": "..." },
  "target_declaration": { "status": "target", "info": { "...": "..." } },
  "surrounding_declaration": { "status": "declaration", "info": { "...": "..." } },
  "total_truncated": false,
  "query_facts": {
    "cache_status": "hit",
    "output_bytes": 4096,
    "timings": {
      "header_import_micros": 0,
      "elaboration_micros": 0,
      "projection_micros": 210,
      "rendering_micros": 75
    }
  }
}
```

Header parse failures return `status: "header_parse_failed"` with the same diagnostics block shape. Selector-level
unavailability and budget exhaustion are reported in `unavailable` / `budget_exceeded` sidebars instead of failing the
whole tool call. Optional context fields are omitted when Lean cannot identify that part of the cursor context.

### `lean_query`

Run a bounded batch of Lean semantic projections against one file. This is the expert/composable form; use `proof_state`
for ordinary proof editing. Selectors are typed objects and each selector has a caller-chosen `id`; results are returned
in an object keyed by those ids.

```jsonc
// request
{
  "file": "LeanRsFixture/SourceRanges.lean",
  "selectors": [
    { "selector": "diagnostics", "id": "diag" },
    { "selector": "proof_state", "id": "state", "line": 8, "column": 3 },
    { "selector": "type_at", "id": "term", "line": 7, "column": 24 },
    { "selector": "references", "id": "refs", "name": "LeanRsFixture.SourceRanges.knownTheorem" },
    { "selector": "declaration_target", "id": "target", "line": 7, "column": 9 },
    { "selector": "surrounding_declaration", "id": "around", "line": 8, "column": 3 }
  ]
}

// result
{
  "status": "results",
  "items": {
    "diag": { "status": "ok", "result": { "kind": "diagnostics", "summary": { ... }, "diagnostics": [] } },
    "state": { "status": "ok", "result": { "kind": "proof_state", "status": "state", "info": { ... } } },
    "term": { "status": "ok", "result": { "kind": "type_at", "status": "term", "type_str": { ... } } }
  },
  "total_truncated": false,
  "query_facts": { "cache_status": "miss", "output_bytes": 8192, "timings": { "...": "..." } }
}
```

Duplicate selector ids and empty selector arrays return `status: "invalid_selectors"`. Text fields use bounded
`{ value, truncated }` objects where rendering can grow. `total_truncated` means the batch-level response budget was
hit; selector-level budget exhaustion returns an item with `status: "budget_exceeded"`.

### `find_references`

```jsonc
// file scope
{ "scope": "file", "file": "LeanRsFixture/Scalars.lean", "name": "Nat.add" }

// project scope restricted to specific files
{ "scope": "project", "name": "Nat.add", "files": ["LeanRsFixture/Scalars.lean"], "limit": 100 }

// result
{
  "status": "ok",
  "references": [
    { "file": "LeanRsFixture/Scalars.lean", "line": 12, "column": 7,
      "end_line": 12, "end_column": 14, "kind": "ref" }
  ],
  "files_scanned": 1,
  "files_skipped": 0,
  "semantic_based": true
}
```

`kind` is `"def"` at binder sites, `"ref"` at use sites. Name matching is exact: pass the fully-qualified form Lean
records. Hits cap at `min(limit, 1000)` and set `truncated: true` when the scan stops early or a per-file query
truncates. Project scope walks every `.lean` file only when `files` is omitted. Malformed scope combinations return
`status: "invalid_request"` as structured data. Per-file failures are reported in omitted-when-empty sidebars:
`unsupported_files`, `header_parse_failed_files`, and `missing_imports_files`. Results are sorted by
`(file, line, column)`.
