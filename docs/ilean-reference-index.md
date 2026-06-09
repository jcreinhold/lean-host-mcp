# The `.ilean` reference-index reader

> Contributor/maintainer note for `src/ilean.rs`: why the `.ilean` reader lives where it does, its narrow interface, and
> the verified on-disk schema.

## Why this exists

Project-scope `find_references` used to re-elaborate every `.lean` module in the worker (~3 s/file → ~27 min on a
~500-module project). `lake build` already writes the answer to disk: one **`.ilean`** file per module under
`<project>/.lake/build/lib/lean/`, the LSP reference index. It records, per name, the definition site and every usage
site *within that module's source*, so "find references to `N`" is a disk read plus a JSON parse, no Lean runtime.

Measured baseline (`references_to` over a 500-module project, worst case — the most-referenced name `Fin`, 7277 hits):
**~565 ms serial**, ~3000× faster than the elaboration sweep and well inside the per-request timeout. So the reader is
deliberately serial: no parallelism, no thread pool, because the worst-case workload doesn't need them.

## Where the reader lives

It is a private module in `lean-host-mcp` (`src/ilean.rs`), not a separate `lean-rs` crate and not a worker capability.
`find_references` is its sole consumer, so a published crate would be a premature boundary; and `.ilean` is pure data,
so routing it through the worker would needlessly drag the `libleanshared` link concern into something that reads JSON
off disk. The reader stays pure Rust + `serde_json` (already dependencies), adding no new dependency and no Lean
linkage, and so preserves the parent ⊥ `libleanshared` invariant. The volatile on-disk format is sealed behind the
version gate below.

## The narrow interface

```rust
pub(crate) fn references_to(project_root: &Path, name: &str) -> ReferenceIndex;
```

- `ReferenceIndex { status, references, modules_scanned, modules_skipped, stale_sources }` — reports as **data**, never
  warns. `status` is `NotBuilt` (no `.lake/build/lib/lean`) or `Present`. A malformed/unreadable/unsupported single file
  is counted in `modules_skipped`, never fatal. `stale_sources` flags contributing modules whose `.lean` is newer than
  its `.ilean` (bounded by the result set, off the hot path).
- `ReferenceLocation { file, start_line, start_column, end_line, end_column, kind }` — 0-based LSP coordinates, mapping
  directly onto `find_references`'s `ReferenceHit`.
- `IleanError` (typed, recoverable) lives one layer down on the private per-file loader so the version gate is
  unit-testable directly. Everything else — the raw JSON shapes — is private to the module.

## Verified v5 schema (Lean v4.31.0-rc1)

Sources: `src/lean/Lean/Data/Lsp/Internal.lean` (`RefIdent`, `RefInfo`, `ModuleRefs`),
`src/lean/Lean/Server/References.lean` (`Ilean` / `Ilean.load`). Confirmed against a real build.

One JSON object per module:

```jsonc
{ "version": 5,
  "module": "Demo.Foo.Bar",
  "directImports": [ ["Std.Data.List", false, true, false] ],   // ignored
  "references": {
    // KEY is a compressed-JSON RefIdent OBJECT (externally tagged), not a flat array:
    "{\"c\":{\"m\":\"Demo.A\",\"n\":\"Demo.A.foo\"}}": {          // const: m=defining module, n=identName
      "definition": [3, 4, 3, 7],                                // [startLine,startCol,endLine,endCol], or null, or +5th parentDecl string
      "usages": [ [5, 2, 5, 5, "Demo.B.bar"], [6, 8, 6, 11] ]
    },
    "{\"f\":{\"m\":\"Demo.B\",\"i\":\"x\"}}": { ... }             // fvar (local) — ignored
  },
  "decls": { ... }                                               // ignored
}
```

To answer "references to `N`": for each module's `.ilean`, keep entries whose key is a **const** with `n == N`, emit the
`definition` (kind `def`, when non-null) and each `usages` entry (kind `ref`) as a location in **that module's source**
(`Demo.A` → `<root>/Demo/A.lean`). The project's own modules are the `.ilean` files found by a **recursive** walk of
`<root>/.lake/build/lib/lean/` (they are nested by namespace; dependency indices live under separate `.lake/packages/*/`
trees and are not visited).

The version gate (`load`) probes `version` before the full parse and returns `IleanError::UnsupportedVersion` for
anything other than `5`, so a future format change can never produce a silent wrong answer.
