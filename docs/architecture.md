# Architecture

One crate, library plus binary. Five layers, top to bottom:

```
main.rs        clap CLI, rmcp stdio entry
server.rs      LeanHostService (rmcp glue)
tools/         lean.rs (6), index.rs (3), position.rs (3), scan.rs (1)
session.rs     SessionHost actor on its own OS thread
index.rs       DeclarationIndex (SQLite, behind the three index tools)
cache.rs       ProcessedFileCache (LRU, behind the three position tools)
```

`session.rs` is the only path to `lean-rs` / `lean-rs-host`. `index.rs` and `cache.rs` sit beside it and serve the tools
that don't want to round-trip through Lean on every call.

## A dedicated thread for all Lean state

`lean-rs` types are `!Send` and carry a `'lean` lifetime anchored to a process-global runtime, so `LeanRuntime`,
`LeanHost`, `LeanCapabilities`, `LeanSession`, and `SessionPool` cannot cross a `tokio` task boundary and cannot live
inside an `Arc<Mutex<…>>` shared between async tasks.

`SessionHost::spawn` reconciles this by starting one OS thread that initialises the runtime, opens the host, loads
capabilities, and then serves requests off a `tokio::mpsc::Receiver` in a blocking loop. Tool handlers submit a
`Request` enum value and `await` a `oneshot` reply, so the caller sees an async actor and the lifetime tangle stays
inside the thread.

The API is closure- and channel-shaped on purpose: a later subprocess-isolated backend can implement the same shape
without changes to the tools.

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

`goal_at_position`, `type_at_position`, and `references_of_name` project the upstream `ProcessedFile` value (four arrays
of info-tree nodes plus diagnostics) into tool-specific results. Re-processing on every cursor move would be wasteful,
so `cache.rs` ships a small LRU of `Arc<ProcessedFile>`, capacity 16, keyed on `(file_path, sha256(contents))`. Any edit
to the source bytes misses structurally. The cache lives on `ToolContext` rather than as a global. `ProcessedFile` is
`Send + Sync + 'static`, so cached entries are read by tool handlers without re-entering the actor thread.

The cache does **not** track the Lean environment. Environment churn (re-import, capability reload) is the
`LeanHostService::new` boundary; the file cache is invalidated only by content change.
