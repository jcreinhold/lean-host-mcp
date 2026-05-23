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

The `rendered` field is the pretty-printed form via the optional `pp_expr` shim. If the loaded capability dylib lacks
`lean_rs_host_meta_pp_expr`, the server falls back to `Expr.toString` and attaches a warning to the envelope; the field
is still populated either way.

### `is_def_eq`

```jsonc
{ "lhs": "1 + 1", "rhs": "2", "imports": [], "transparency": "reducible" }
// result
{ "status": "Ok", "definitionally_equal": true, "rendered": null, "failure": null }
```

`transparency` is optional and accepts `default` | `reducible` | `instances` | `all` (default: `default`). Picks the
reducibility view the underlying `Meta.isDefEq` runs under — the same two terms can be def-eq under one setting and not
another.

### `hover_by_name`

```jsonc
{ "name": "Nat.add_zero" }
// result
{ "status": "found",   "name": "Nat.add_zero", "kind": "theorem",
  "type_signature": "∀ (n : Nat), n + 0 = n", "source": { ... } }
{ "status": "missing", "name": "Nat.foo_bar" }
```

`type_signature` is rendered via `Expr.toString` — cheap and deterministic, but without notation. For the user-visible
pretty form, call `infer_type` on the declaration's term.

## Project scan (`src/tools/scan.rs`)

### `project_scan`

```jsonc
{ "preset": "sorry" }
{ "preset": "custom", "pattern": "@\\[simp\\]", "limit": 100 }
```

Presets: `sorry` | `admit` | `axiom` | `set_option` | `custom`.

## Index tools (`src/tools/index.rs`)

All three rebuild a SQLite-backed declaration index on first call after the Lake manifest changes. Subsequent calls
reuse the cache. Limits default to 50, clamp at 500. Result rows have shape
`{ "name", "kind", "type_signature": "..." | null, "source": { ... } | null }`.

### `find_symbol`

Case-insensitive substring on declaration names.

```jsonc
{ "query": "add_zero", "limit": 20 }
```

### `find_lemma`

As `find_symbol`, restricted to `kind = "theorem"`.

```jsonc
{ "query": "add_zero" }
```

### `outline`

Name-prefix listing, ordered by name. Omit `module_prefix` to walk the whole table.

```jsonc
{ "module_prefix": "Nat.", "limit": 200 }
```
