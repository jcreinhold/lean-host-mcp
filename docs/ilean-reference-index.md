# The `.ilean` reference-index reader

> Contributor/maintainer note for `src/ilean.rs`: why the `.ilean` reader lives where it does, its narrow interface, and
> the verified on-disk schema.

## Why this exists

Project-scope reference lookup, now exposed as `lean_lookup(kind = "references")`, used to re-elaborate every `.lean`
module in the worker (~3 s/file → ~27 min on a ~500-module project). `lake build` already writes the answer to disk:
one **`.ilean`** file per module under
`<project>/.lake/build/lib/lean/`, the LSP reference index. It records, per name, the definition site and every usage
site *within that module's source*, so "find references to `N`" is a disk read plus a JSON parse, no Lean runtime.

## What it costs

Measured with `benches/ilean_reference_scan.rs` over a 1431-module project (76 MB of `.ilean`), worst case — the
most-referenced name in the corpus, `Fin`, 9095 hits before the result cap:

| Arm | Before | After |
| --- | --- | --- |
| whole project, hot name | 427 ms | ~120 ms |
| whole project, name with no hits | 478 ms | ~136 ms |
| one requested file | 271 ms | ~0.5 ms |

A phase split over the same corpus put ~95% of the original cost in the JSON parse (`stat` 0.010 s, read 0.066 s, parse
1.32 s), so all three wins come from parsing less, not from remembering more — the reader holds **no** cache and no
retained state between calls. In order: the document is parsed once rather than twice (see the version gate below),
`references` and `decls` are read by separate projections so neither entry point materializes the other's subtree, the
`references` map is filtered *during* deserialization so allocation is proportional to hits rather than to the ~400k
entries in the corpus, and a `files`-restricted request reads only the indices that can contribute.

The reader is deliberately **serial**: no parallelism, no thread pool. What remains is one non-recursive byte scan per
file plus key unescaping, comfortably inside the per-request timeout, and parallelising it would need a new dependency
and would make `stale_sources` order nondeterministic (it is user-visible, truncated to three names in the freshness
warning). Revisit if the whole-project arm passes ~1 s on a real corpus. The scan runs under `spawn_blocking`, since it
is file I/O and parsing from first byte to last and must not hold a tokio worker.

## Where the reader lives

It is a private module in `lean-host-mcp` (`src/ilean.rs`), not a separate `lean-rs` crate and not a worker capability.
The references lookup mode is its sole consumer, so a published crate would be a premature boundary; and `.ilean` is pure
data, so routing it through the worker would needlessly drag the `libleanshared` link concern into something that reads
JSON off disk. The reader stays pure Rust + `serde_json` (already dependencies), adding no new dependency and no Lean
linkage, and so preserves the parent ⊥ `libleanshared` invariant. The volatile on-disk format is sealed behind the
version gate below.

## The narrow interface

```rust
pub(crate) fn references_to(project_root: &Path, name: &str, files: Option<&[PathBuf]>) -> ReferenceIndex;
pub(crate) fn declarations_in_module(project_root: &Path, module: &str) -> ModuleDeclarationIndex;
```

- `files` restricts the answer to references *occurring in* those sources. Because an `.ilean` records only the
  references occurring in its own module's source, that is also a restriction on which indices can contribute — so the
  narrowing is the reader's own business, and callers never learn how a source path maps to an index path. When a path
  cannot be inverted with certainty (a dotted component, a non-`.lean` extension, a path outside the root, or a computed
  index that does not exist — the last of which covers Lake's filename mangling, e.g. module `«kan-lint-style»` indexing
  as `kan-lint-style.ilean`) the whole request falls back to the full walk, so a narrowed answer is never smaller than an
  unnarrowed one.
- `ReferenceIndex { status, references, modules_scanned, modules_skipped, stale_sources }` — reports as **data**, never
  warns. `status` is `NotBuilt` (no `.lake/build/lib/lean`) or `Present`. A malformed/unreadable/unsupported single file
  is counted in `modules_skipped`, never fatal. `stale_sources` flags contributing modules whose `.lean` is newer than
  its `.ilean` (bounded by the result set, off the hot path).
- `ReferenceLocation { file, start_line, start_column, end_line, end_column, kind }` — 0-based LSP coordinates, mapping
  directly onto the references mode's `ReferenceHit`.
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
  "decls": { "Demo.A.foo": [3,4,3,7, 3,4,3,7] }                  // declRange ++ selectionRange
}
```

`references` is ~97% of the bytes and `decls` ~3%, and the two entry points want opposite halves, so they are read by
two independent projections rather than one shared document type. Neither can be broken by garbage in the other's
subtree.

To answer "references to `N`": for each module's `.ilean`, keep entries whose key is a **const** with `n == N`, emit the
`definition` (kind `def`, when non-null) and each `usages` entry (kind `ref`) as a location in **that module's source**
(`Demo.A` → `<root>/Demo/A.lean`). The project's own modules are the `.ilean` files found by a **recursive** walk of
`<root>/.lake/build/lib/lean/` (they are nested by namespace; dependency indices live under separate `.lake/packages/*/`
trees and are not visited).

The version gate returns `IleanError::UnsupportedVersion` for anything other than `5`, so a future format change can
never produce a silent wrong answer. It runs **after** the parse, not before it: Lean's `Json` object is an `RBNode`, so
`Json.compress` emits keys sorted and `version` is the **last** key — at byte 116,287 of 116,299 in a sampled real file.
A "probe the version first" pass therefore read the entire document anyway, doubling the cost of every file for a check
that could just as well run at the end. A document that parses but carries the wrong version is discarded whole,
including hits already collected from it.

The one thing the pre-parse probe did that a post-parse check cannot is classify a *future* version whose shape this
reader cannot parse at all. That verdict is preserved by re-probing on the failure path only, so it costs nothing on
files that parse. `version_may_be_the_last_field` and `future_version_with_unreadable_shape_is_unsupported` in
`src/ilean.rs` pin both halves; the older fixtures put `version` first and resemble no real `.ilean`.
