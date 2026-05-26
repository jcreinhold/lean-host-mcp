# Architecture

One crate, library plus two binaries (the MCP server and a tiny worker child). Layers, top to bottom:

```
main.rs         clap CLI, rmcp stdio entry
server.rs       LeanHostService (rmcp glue)
tools/          lean.rs (6), index.rs (3), position.rs (4), scan.rs (1)
project.rs      LeanProject—closure-channel actor; owns the LeanWorkerCapability,
                the DeclarationIndex, and the ProcessedFileCache for one Lake project
projections.rs  pure data-shuffle helpers from lean-rs-worker shapes into the MCP wire
lake_meta.rs    LakeProjectMeta: minimal Lake-project description
index.rs        DeclarationIndex (SQLite, behind the three index tools)
cache.rs        ProcessedFileCache (LRU, behind the four position tools)
envelope.rs     Response<T> = { result, freshness, warnings, next_actions }
bin/worker.rs   2-line entry: lean_rs_worker::run_worker_child_stdio()
```

`project.rs` is the only path to `lean-rs-worker` (the parent never sees `lean-rs` or `lean-rs-host` directly—those
live inside the worker child). `index.rs` and `cache.rs` are owned by `LeanProject` and serve the tools that don't want
to round-trip through Lean on every call. A `ProjectBroker` (`broker.rs`) maintains an LRU pool of `Arc<LeanProject>`
so one server can multiplex several Lake projects in the same Claude session.

## The broker: pool, invalidation, identity

`ProjectBroker` is the only thing the tool layer touches when it needs a `LeanProject`. Three knobs:

- **`max_projects`** (`LEAN_HOST_MCP_MAX_PROJECTS`, default 4). The pool is an `lru::LruCache<PathBuf,
  Arc<LeanProject>>`; a miss when the cache is full evicts the LRU entry, which is `shutdown()`'d before the new project
  is spawned.
- **`idle_timeout`** (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`, default 600). A `tokio::spawn`'d reaper task held by `Weak`
  fires every 60 s and drops entries whose `last_used` is past the window. Set to 0 to disable.
- **Manifest invalidation.** Every cache hit re-fingerprints `lake-manifest.json`; a mismatch evicts and re-opens. Cost
  is one ≤ 50 KB SHA-256 per tool call.

**The mutex is never held across `LeanProject::open` or `project.submit`.** The miss path drops the broker lock,
spawns the new project, then reacquires the lock briefly to insert. Concurrent misses for the same path race on insert
and the loser's project is shut down on the dispatch path; concurrent calls against different projects parallelize
fully.

Identity travels through `Freshness.session_id`: the project actor's UUID, allocated once at `LeanProject::open` and
stable for that actor's lifetime. The only events that change it are the three eviction triggers above. Clients can
detect "my project was silently restarted" by comparing `session_id` across calls—that's the one observable
side-effect of the pool decisions, and the multi-project tests use it as their identity signal.

## Multi-toolchain dispatch

The workspace is split into two members so the parent stays free of `libleanshared`:

- `crates/lean-host-mcp/` depends on `lean-rs-worker-parent`, which carries no `lean-rs` / `lean-rs-host` /
  `lean-rs-sys` link in its closure (`lean-toolchain`'s `lean-rs-sys` dep uses `metadata-only`, whose `build.rs` exits
  before emitting link directives). `otool -L target/release/lean-host-mcp` shows only `libSystem` + `libiconv`.
- `crates/lean-host-mcp-worker/` depends on `lean-rs-worker-child` and is the only crate that links `libleanshared`.
  Its `build.rs` reads `LEAN_HOST_MCP_TARGET_TOOLCHAIN` (set by `lean-host-mcp install-worker`) and bakes
  `~/.elan/toolchains/leanprover--lean4---<id>/lib/lean` into the binary's rpath.

`crates/lean-host-mcp/src/toolchain.rs` resolves a project's `lean-toolchain` pin to one of the per-toolchain worker
binaries installed under `~/.local/share/lean-host-mcp/workers/<id>/lean-host-mcp-worker`.
`LeanProject::open` invokes `WorkerBinary::resolve_for` up front and passes the result to
`LeanWorkerChild::path(...)`; a missing worker surfaces as `ServerError::BadProject` whose message includes the exact
`lean-host-mcp install-worker --toolchain <id>` command needed.

**Feature unification caveat.** `cargo build --workspace` would unify `lean-rs-sys`'s features across both members and
silently relink `libleanshared` into the parent. Always build per-member (`cargo build -p lean-host-mcp` /
`cargo build -p lean-host-mcp-worker`); CI runs them as separate jobs and asserts
`! otool -L target/release/lean-host-mcp | grep -q libleanshared`.

### Manual smoke test

```sh
lean-host-mcp install-worker --toolchain v4.30.0-rc2
lean-host-mcp install-worker --list                                          # see one row
LEAN_HOST_MCP_PROJECT=/path/to/project-on-v4.30 lean-host-mcp                # serves
LEAN_HOST_MCP_PROJECT=/path/to/project-on-v4.29 lean-host-mcp                # error includes install_cmd
lean-host-mcp install-worker --toolchain v4.29.1
# same server, both projects work
```

## A supervised worker child for all Lean state

Lean lives in a child process—the `lean-host-mcp-worker` binary, resolved per-toolchain by `WorkerBinary::resolve_for`
and handed to the supervisor as `LeanWorkerChild::path(...)`. A wedged tactic, typeclass loop, or OOM mid-elaboration
kills that child; the supervisor restarts it on the next request rather than taking down the MCP server.

The parent sees only `LeanWorkerCapability` (Send) and short-lived `LeanWorkerSession<'_>` borrows that don't escape
their owning stack frame. The "one owner at a time" invariant holds: `LeanWorkerCapability` cannot be shared across
tokio tasks.

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

Per-request session-open is fine: subsequent opens with the same import set reuse the child's module cache, so only
the first open per import set pays the load cost. The `worker_roundtrip` bench pins this.

## The envelope contract

Every tool wraps its result in `Response<T>` from `envelope.rs`. That struct is the only thing the entire tool layer
agrees on, and it hides three decisions that are still in motion:

1. **What "freshness" means.** Today: lake root, imports, session id, toolchain label. May grow file-version vectors
   or build ids.
2. **What a warning looks like.** A plain string today; possibly a structured `{ code, message }` once there's a stable
   warning catalogue.
3. **Whether `next_actions` are present.** A hint surface for the LLM client. Tools sprinkle them; the envelope decides
   whether to serialise.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) live inside `result`, not as MCP errors.
`ServerError` is reserved for infrastructure: worker thread gone, runtime init failed, Lake project unusable.

## The `DeclarationIndex` boundary

`find_symbol`, `find_lemma`, and `outline` all answer "what declarations match X" against the open Lake project. They
share one piece of state: a SQLite database under the user's cache directory, keyed by Lake-manifest hash. `index.rs`
owns that boundary in full—schema, fingerprinting, bulk rebuild, the seven read methods the tools consume. Nothing past
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
functions on `cache.rs`; linear scan is fast enough at typical file sizes (the `position_lookup_after_cache_warm` bench
targets ≤ 50 µs per query).

The cache is populated through `LeanProject`'s worker actor calling `process_module` (the header-aware worker shim).
Files whose header references modules the session's open env doesn't have are still cached—the body's partial projection
is real data, and the `MissingImports` signal travels alongside on the envelope. Header parse failures and `Unsupported`
outcomes are not cached.

The cache does **not** track the Lean environment. Environment churn (re-import, capability reload) is the
`LeanHostService::new` boundary; the file cache is invalidated only by content change.
