# Tool catalogue

All tools return the same envelope shape; only `result` differs.

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

Full elaborate + addDecl. Returns Lean's `LeanKernelOutcome` projected.

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

`Meta.inferType` / `Meta.whnf` over a term that's elaborated first.

```jsonc
{ "term": "Nat.succ 0", "imports": [] }
```

The `rendered` field in the result is a placeholder string in v0.1 — the
underlying `LeanExpr` is opaque across the worker channel boundary. v0.2
backfills the pretty-printed form once `lean-rs` exposes a shim.

### `is_def_eq`

```jsonc
{ "lhs": "1 + 1", "rhs": "2", "imports": [], "transparency": "reducible" }
// result
{ "status": "Ok", "definitionally_equal": true, "rendered": null, "failure": null }
```

`transparency` is optional and accepts `default` | `reducible` |
`instances` | `all` (default: `default`). Picks the reducibility view
the underlying `Meta.isDefEq` runs under — the same two terms can be
def-eq under one setting and not another.

### `hover_by_name`

```jsonc
{ "name": "Nat.add_zero" }
// result
{ "status": "found",   "name": "Nat.add_zero", "kind": "theorem", "source": { ... } }
{ "status": "missing", "name": "Nat.foo_bar" }
```

`type_signature` is always `None` in v0.1 (see `infer_type` note above).

## Project scan (`src/tools/scan.rs`)

### `project_scan`

```jsonc
{ "preset": "sorry" }
{ "preset": "custom", "pattern": "@\\[simp\\]", "limit": 100 }
```

Presets: `sorry` | `admit` | `axiom` | `set_option` | `custom`.
