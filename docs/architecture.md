# Architecture

One crate, library plus two binaries (the MCP server and a tiny worker child). Layers, top to bottom:

```
main.rs         clap CLI, rmcp stdio entry
server.rs       LeanHostService (rmcp glue)
tools/          lean.rs (6), index.rs (3), position.rs (4), scan.rs (1)
project.rs      LeanProject—closure-channel actor; owns the LeanWorkerCapability,
                the DeclarationIndex, and the ProcessedFileCache for one Lake project
projections.rs  pure data-shuffle helpers from lean-rs-worker shapes into the MCP wire
lake_meta.rs    LakeProjectMeta — minimal Lake-project description
index.rs        DeclarationIndex (SQLite, behind the three index tools)
cache.rs        ProcessedFileCache (LRU, behind the four position tools)
envelope.rs     Response<T> = { result, freshness, warnings, next_actions }
bin/worker.rs   2-line entry: lean_rs_worker::run_worker_child_stdio()
```

`project.rs` is the only path to `lean-rs-worker` (the parent never sees `lean-rs` or `lean-rs-host` directly — those
live inside the worker child). `index.rs` and `cache.rs` are owned by `LeanProject` and serve the tools that don't want
to round-trip through Lean on every call. There is one `LeanProject` per server today; the abstraction is shaped for
later multi-project dispatch through a broker layer.

## Toolchain linkage

The parent binary (`lean-host-mcp`) transitively links `libleanshared` through `lean-rs-worker` → `lean-rs` →
`lean-rs-sys`; `otool -L target/release/lean-host-mcp` shows `@rpath/libleanshared.dylib`. `build.rs`'s rpath emission
is therefore load-bearing for the parent, not only the worker child. Multi-toolchain support has to either thread
per-toolchain launchers around the parent or split the parent off the worker crate to drop the transitive link.

## A supervised worker child for all Lean state

Lean lives in a child process—the `lean-host-mcp-worker` binary, resolved sibling-to-the-parent by
`LeanWorkerChild::sibling("lean-host-mcp-worker")`. A wedged tactic, a typeclass loop, or OOM mid-elaboration kills that
child; the supervisor restarts it on the next request rather than taking down the MCP server.

The parent sees only `LeanWorkerCapability` (Send) and short-lived `LeanWorkerSession<'_>` borrows that don't escape
their owning stack frame. The `'lean` lifetime tangle the in-process implementation had to navigate is gone, but the
"one owner at a time" invariant remains—`LeanWorkerCapability` cannot be shared across tokio tasks.

`LeanProject::open` parks the capability on a dedicated OS thread named
`"lean-host-mcp/project/<canonical_root_basename>"` and serves a `tokio::mpsc::Receiver<Job>` in a blocking loop. Each
tool handler calls `project.submit(|cap| { ... })` to ship a typed closure to that thread; the closure opens a fresh
session via `cap.open_session_with_imports(...)`, calls the worker, projects the worker's wire-stable result type into
the MCP-stable wire shape via `projections.rs`, and replies via `oneshot`. No `Request` enum, no `WorkerState`—adding a
new tool is one closure dispatched through `project.submit`.

```rust
type Job = Box<dyn FnOnce(&mut LeanWorkerCapability) + Send + 'static>;

// dispatch loop on the dedicated thread:
while let Some(job) = rx.blocking_recv() {
    job(&mut capability);
}
```

Per-request session-open is fine for v0.2: subsequent opens with the same import set reuse the child's module cache, so
only the first open per import set pays the load cost. The `worker_roundtrip` bench pins this.

## The envelope contract

Every tool wraps its result in `Response<T>` from `envelope.rs`. That struct is the only thing the entire tool layer
agrees on, and it hides three decisions that are still in motion:

1. **What "freshness" means.** Today: lake root, imports, session id, toolchain label. Tomorrow may add file-version
   vectors or build ids.
2. **What a warning looks like.** A plain string today; possibly a structured `{ code, message }` once there's a stable
   warning catalogue.
3. **Whether `next_actions` are present.** A hint surface for the LLM client. Tools sprinkle them; the envelope decides
   whether to serialise.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) live inside `result`, not as MCP errors.
`ServerError` is reserved for infrastructure: worker thread gone, runtime init failed, Lake project unusable.

## The `DeclarationIndex` boundary

`find_symbol`, `find_lemma`, and `outline` all answer "what declarations match X" against the open Lake project. They
share one piece of state: a SQLite database under the user's cache directory, keyed by Lake-manifest hash. `index.rs`
owns that boundary in full: schema, fingerprinting, bulk rebuild, the seven read methods the tools consume. Nothing past
the module sees `rusqlite` or `sha2`; a fourth caller adds a method here rather than writing SQL.

Rebuilds are on-demand and gated by the SHA-256 of `lake-manifest.json`. The session walks the live environment via
`LeanSession::list_declarations_strings` followed by `declaration_kind_bulk` and `declaration_type_bulk`, the index
commits in one transaction, and the fingerprint is stamped last so a crashed rebuild leaves the index detectably stale
rather than partially populated.

## The file-processing cache

`goal_at_position`, `type_at_position`, `references_of_name`, and `file_diagnostics` project the worker's
`LeanWorkerProcessedFile` value (four arrays of info-tree nodes plus diagnostics) into tool-specific results.
Re-processing on every cursor move would be wasteful, so `cache.rs` ships a small LRU of `Arc<LeanWorkerProcessedFile>`,
capacity 16, keyed on `(file_path, sha256(contents))`. Any edit to the source bytes misses structurally. The cache lives
inside `LeanProject`, so within one cache instance every entry necessarily shares the project's
`(canonical_root, package, library, default_imports)`; import-collision safety is structural rather than enforced by the
key. `LeanWorkerProcessedFile` is owned data (`Send + Sync + 'static`), so cached entries are read by tool handlers
without re-entering the actor thread. Position lookup helpers (`tactic_at`, `term_at`, `references_of`) are free
functions on `cache.rs`; linear scan is fast enough at v0.2 file sizes (the `position_lookup_after_cache_warm` bench
targets ≤ 50 µs per query).

The cache is populated through `LeanProject`'s worker actor calling `process_module` (the header-aware worker shim).
Files whose header references modules the session's open env doesn't have are still cached—the body's partial projection
is real data, and the `MissingImports` signal travels alongside on the envelope. Header parse failures and `Unsupported`
outcomes are not cached.

The cache does **not** track the Lean environment. Environment churn (re-import, capability reload) is the
`LeanHostService::new` boundary; the file cache is invalidated only by content change.
