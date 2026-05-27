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

Lean session and declaration tools also accept an `imports` field. It is the complete per-call import vector. An empty array
means the call asks for no extra imports beyond the worker's base environment.

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

### `hover_by_name`

```jsonc
// request
{ "name": "Nat.add_zero", "imports": [], "max_type_bytes": 8192 }

// result
{ "status": "found",   "name": "Nat.add_zero", "kind": "theorem",
  "type_signature": { "value": "∀ (n : Nat), n + 0 = n", "truncated": false },
  "source": { ... } }
{ "status": "missing", "name": "Nat.foo_bar" }
```

`hover_by_name` is a compatibility alias for `type_of_name`: it returns one declaration's type under a hard byte cap.

### `type_of_name`

```jsonc
// request
{ "name": "Nat.add_zero", "imports": [], "max_type_bytes": 8192 }

// result
{ "status": "found", "name": "Nat.add_zero", "kind": "theorem",
  "type_signature": { "value": "∀ (n : Nat), n + 0 = n", "truncated": false },
  "source": { ... } }
{ "status": "missing", "name": "Nat.foo_bar" }
```

`max_type_bytes` defaults to 8192 and clamps to 65536. This tool is the only declaration-name tool that renders a type.

### `search_declarations`

Case-insensitive substring search over declaration names. Results are metadata-only; no declaration types are rendered
or indexed.

```jsonc
// request
{ "query": "add_zero", "kind": "theorem", "imports": [], "limit": 20, "include_source": true }

// result
{ "declarations": [{ "name": "Nat.add_zero", "kind": "theorem", "source": { ... } }], "truncated": false }
```

`kind` is optional. Limits default to 20 and clamp to 100.

## Project scan (`src/tools/scan.rs`)

### `project_scan`

Filesystem regex sweep over the project's `.lean` files. No Lean dependency; results are textual matches, not elaborator
output. Its freshness envelope reports `imports: []`.

```jsonc
{ "preset": "sorry" }
{ "preset": "custom", "pattern": "@\\[simp\\]", "limit": 100 }
```

Presets: `sorry | admit | axiom | set_option | custom`.

## Position tools (`src/tools/position.rs`)

The position tools drive `process_module_query`: they read the file, hand the full source (header + body) to Lean's
frontend, and ask for one bounded projection. The file's own `import` declarations are parsed by Lean and validated
against the server's open env; mismatch surfaces as an envelope `warnings` entry for single-file tools or a result
sidebar for `references_in_project`. Query results are cached against `(file_path, sha256(contents), query_kind)`, so an
identical repeated query on the same bytes reuses the bounded response; an edit invalidates structurally.

`process_module_query` is an **optional** capability shim. When the loaded dylib lacks the
`lean_rs_host_process_module_query` shim, position tools answer `{ "status": "unsupported" }` cleanly; the tools never
raise.

All line and column inputs are **1-indexed**. Result spans use the same convention.

### `goal_at_position`

```jsonc
// request
{ "file": "LeanRsFixture/SourceRanges.lean", "line": 8, "column": 3 }

// result: tactic context found
{
  "status": "goal",
  "goals_before": ["⊢ True"],
  "goals_after":  [],
  "span": { "start_line": 8, "start_column": 3, "end_line": 8, "end_column": 10 },
  "truncated": false
}

// result: no tactic at cursor
{ "status": "no_tactic_context" }

// result: file's header did not parse
{ "status": "header_parse_failed",
  "diagnostics": { "diagnostics": [...], "truncated": false } }

// result: worker host shims missing the shim
{ "status": "unsupported" }
```

`file` resolves relative to the resolved project root when not absolute. Goals are pre-rendered by Lean's `Meta.ppGoal`
inside the elaboration context; the strings are diagnostic text only. When the file's header imports modules the
server's open env doesn't have, the result is still returned but the envelope's `warnings` array names the missing
modules.

### `type_at_position`

```jsonc
// request
{ "file": "LeanRsFixture/SourceRanges.lean", "line": 7, "column": 24 }

// result: innermost term found
{
  "status": "term",
  "expr":          { "value": "True", "truncated": false },
  "type_str":      { "value": "Prop", "truncated": false },
  "expected_type": null,
  "span": { "start_line": 7, "start_column": 24, "end_line": 7, "end_column": 28 }
}

// result: no term recorded
{ "status": "no_term" }

// result: file's header did not parse
{ "status": "header_parse_failed",
  "diagnostics": { "diagnostics": [...], "truncated": false } }

// result: worker host shims missing the shim
{ "status": "unsupported" }
```

`expr`, `type_str`, and `expected_type` are bounded rendered text objects. `expected_type` is set only at sites where the
elaborator recorded one, such as coercion sites. The same `MissingImports` warning behavior as `goal_at_position`.

### `references_in_file`

```jsonc
// request
{ "file": "LeanRsFixture/Scalars.lean", "name": "Nat.add" }

// result
{
  "references": [
    { "file": "LeanRsFixture/Scalars.lean", "line": 12, "column": 7,
      "end_line": 12, "end_column": 14, "kind": "ref" }
  ]
}
```

`kind` is `"def"` at binder sites, `"ref"` at use sites. Name matching is exact: pass the fully-qualified form Lean
records. The result includes `truncated` when the upstream reference projection hit its per-file budget.

### `references_in_project`

```jsonc
// request: explicitly search every project .lean file
{ "name": "Nat.add", "limit": 1000 }

// request: restrict to specific files
{ "name": "Nat.add", "files": ["LeanRsFixture/Scalars.lean"], "limit": 100 }

// result
{
  "references": [
    { "file": "LeanRsFixture/Scalars.lean", "line": 12, "column": 7,
      "end_line": 12, "end_column": 14, "kind": "ref" }
  ],
  "files_scanned": 1
}
```

Hits cap at `min(limit, 1000)` and set `truncated: true` when the scan stops early or a per-file query truncates. The
walk continues past per-file failures; three sidebars (all omitted when empty) report them: `unsupported_files` (dylib
lacks the shim), `header_parse_failed_files` (`{ file, diagnostics }`), `missing_imports_files` (`{ file, missing: [...]
}`). Results are sorted by `(file, line, column)`.

### `file_diagnostics`

Elaboration diagnostics—errors, warnings, info—for a `.lean` file. This is a diagnostics-only projection: term/type,
tactic, and name-reference payloads are not rendered or transported.

```jsonc
// request
{ "file": "LeanRsFixture/SourceRanges.lean" }

// result: elaboration completed; diagnostics may be empty, info-only, or carry errors
{
  "status": "elaborated",
  "summary": { "errors": 1, "warnings": 0, "info": 0 },
  "diagnostics": [
    { "severity": "error", "message": "type mismatch ...",
      "position": { "line": 12, "column": 9, "end_line": 12, "end_column": 13 } }
  ],
  "truncated": false
}

// result: file's header did not parse; body never elaborated; diagnostics are the parser's
{ "status": "header_parse_failed",
  "summary": { "errors": 1, "warnings": 0, "info": 0 },
  "diagnostics": [...], "truncated": false }

// result: worker host shims missing the module-query shim
{ "status": "unsupported" }
```

`status: "elaborated"` only means we got far enough to collect diagnostics; `summary.errors > 0` is the real "does this
file have problems?" signal. `severity` is lowercase (`"error" | "warning" | "info"`). `diagnostics` is sorted by
`(line, column)`. The per-diagnostic `file` field is omitted for Lean's synthetic source labels (almost always).
`truncated` is true only when Lean hit the diagnostic byte budget; the list is then a prefix. `Elaborated` and
`HeaderParseFailed` share the same shape (`summary` + `diagnostics` + `truncated`) so callers render one structure. The
same `MissingImports` envelope-warning behaviour as the cursor-driven tools applies.
