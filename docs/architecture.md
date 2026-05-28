# Architecture

One crate, library plus two binaries (the MCP server and a tiny worker child). Layers, top to bottom:

```
main.rs         clap CLI, rmcp stdio entry
server.rs       LeanHostService (rmcp glue)
tools/          lean.rs (6), index.rs (3), position.rs (5), scan.rs (1)
project.rs      LeanProject—closure-channel actor; owns the LeanWorkerHostHandle,
                the DeclarationIndex, and the ModuleQueryCache for one Lake project
projections.rs  pure data-shuffle helpers from lean-rs-worker shapes into the MCP wire
lake_meta.rs    LakeProjectMeta: minimal Lake-project description
index.rs        DeclarationIndex (SQLite, behind the three index tools)
cache.rs        ModuleQueryCache (LRU, behind the bounded position tools)
envelope.rs     Response<T> = { result, freshness, warnings, next_actions }
bin/worker.rs   2-line entry: lean_rs_worker_child::run_worker_child_stdio()
```

`project.rs` is the only path to `lean-rs-worker` (the parent never sees `lean-rs` or `lean-rs-host` directly—those live
inside the worker child). `index.rs` and `cache.rs` are owned by `LeanProject` and serve the tools that don't want to
round-trip through Lean on every call. A `ProjectBroker` (`broker.rs`) maintains an LRU pool of `Arc<LeanProject>` so
one server can multiplex several Lake projects in the same Claude session.

## The broker: pool, invalidation, identity

`ProjectBroker` is the only thing the tool layer touches when it needs a `LeanProject`. Three knobs:

- **`max_projects`** (`LEAN_HOST_MCP_MAX_PROJECTS`, default 4). The pool is an `lru::LruCache<PathBuf,
  Arc<LeanProject>>`; a miss when the cache is full evicts the LRU entry, which is `shutdown()`'d before the new project
  is spawned.
- **`idle_timeout`** (`LEAN_HOST_MCP_IDLE_TIMEOUT_SECS`, default 600). A `tokio::spawn`'d reaper task held by `Weak`
  fires every 60 s and drops entries whose `last_used` is past the window. Set to 0 to disable.
- **Manifest invalidation.** Every cache hit re-fingerprints `lake-manifest.json`; a mismatch evicts and re-opens. Cost
  is one ≤ 50 KB SHA-256 per tool call.

**The mutex is never held across `LeanProject::open` or `project.submit`.** The miss path drops the broker lock, spawns
the new project, then reacquires the lock briefly to insert. Concurrent misses for the same path race on insert and the
loser's project is shut down on the dispatch path; concurrent calls against different projects parallelize fully.

Identity travels through `Freshness.session_id`: the project actor's UUID, allocated once at `LeanProject::open` and
stable for that actor's lifetime. The only events that change it are the three eviction triggers above. Clients can
detect "my project was silently restarted" by comparing `session_id` across calls—that's the one observable side-effect
of the pool decisions, and the multi-project tests use it as their identity signal.

## Multi-toolchain dispatch

The workspace is split into two members so the parent stays free of `libleanshared`:

- `crates/lean-host-mcp/` depends on `lean-rs-worker-parent`, which carries no `lean-rs` / `lean-rs-host` /
  `lean-rs-sys` link in its closure (`lean-toolchain`'s `lean-rs-sys` dep uses `metadata-only`, whose `build.rs` exits
  before emitting link directives). `otool -L target/release/lean-host-mcp` shows only `libSystem` + `libiconv`.
- `crates/lean-host-mcp-worker/` depends on `lean-rs-worker-child` and is the only crate that links `libleanshared`. Its
  `build.rs` reads `LEAN_HOST_MCP_TARGET_TOOLCHAIN` (set by `lean-host-mcp install-worker`) and bakes
  `~/.elan/toolchains/leanprover--lean4---<id>/lib/lean` into the binary's rpath.

`crates/lean-host-mcp/src/toolchain.rs` resolves a project's `lean-toolchain` pin to one of the per-toolchain worker
binaries installed under `~/.local/share/lean-host-mcp/workers/<id>/lean-host-mcp-worker`. `LeanProject::open` invokes
`WorkerBinary::resolve_for` up front and passes the result to `LeanWorkerChild::path(...)`; a missing worker surfaces as
`ServerError::BadProject` whose message includes the exact `lean-host-mcp install-worker --toolchain <id>` command
needed.

**Feature unification caveat.** `cargo build --workspace` would unify `lean-rs-sys`'s features across both members and
silently relink `libleanshared` into the parent. Always build per-member (`cargo build -p lean-host-mcp` /
`cargo build -p lean-host-mcp-worker`); CI runs them as separate jobs and asserts
`! otool -L target/release/lean-host-mcp | grep -q libleanshared`.

### Manual smoke test

```sh
lean-host-mcp install-worker --toolchain v4.30.0
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

The parent sees only `LeanWorkerHostHandle` (Send) and short-lived `LeanWorkerSession<'_>` borrows that don't escape
their owning stack frame. The "one owner at a time" invariant holds: the host handle is parked on one actor thread and
is not shared across tokio tasks.

`LeanProject::open` opens a shims-only host handle with `LeanWorkerHostHandleBuilder::shims_only(...)`, then parks that
handle on a dedicated OS thread named `"lean-host-mcp/project/<canonical_root_basename>"` and serves a
`tokio::mpsc::Receiver<Job>` in a blocking loop. Each tool handler calls `project.submit(|handle| { ... })` to ship a
typed closure to that thread; the closure opens a fresh session via `handle.open_session_with_imports(...)`, calls the
worker, projects the worker's wire-stable result type into the MCP-stable wire shape via `projections.rs`, and replies
via `oneshot`. No `Request` enum, no `WorkerState`—adding a new tool is one closure dispatched through `project.submit`.

```rust
type Job = Box<dyn FnOnce(&mut LeanWorkerHostHandle) + Send + 'static>;

// dispatch loop on the dedicated thread:
while let Some(job) = rx.blocking_recv() {
    job(&mut handle);
}
```

The shims-only bootstrap loads the bundled host shim/interoperability dylibs and never builds or `dlopen`s the consumer
project's `:shared` facet. A broken project module can still fail a session that explicitly imports it, but it does not
prevent the MCP from opening sessions for unrelated imports or collecting diagnostics for the broken file.

Per-request session-open is fine: subsequent opens with the same import set reuse the child's module cache, so only the
first open per import set pays the load cost. The `worker_roundtrip` bench pins this.

## The envelope contract

Every tool wraps its result in `Response<T>` from `envelope.rs`. That struct is the only thing the entire tool layer
agrees on, and it hides three decisions that are still in motion:

1. **What "freshness" means.** Today: lake root, imports, session id, toolchain label. May grow file-version vectors or
   build ids.
2. **What a warning looks like.** A plain string today; possibly a structured `{ code, message }` once there's a stable
   warning catalogue.
3. **Whether `next_actions` are present.** A hint surface for the LLM client. Tools sprinkle them; the envelope decides
   whether to serialise.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) live inside `result`, not as MCP errors.
`ServerError` is reserved for infrastructure: worker thread gone, runtime init failed, Lake project unusable.

## Bounded declaration inspection

Declaration inspection is worker-backed and intentionally single-name. `inspect_declaration` resolves either an explicit
declaration name or one cursor-selected declaration, then returns bounded statement text, documentation, source/module
metadata, cheap proof-search facts, and private/generated/internal flags. Rendered text fields carry their own
truncation flags and are capped before they cross the worker boundary.

The public MCP declaration surface does not expose raw declaration search or type-only lookup. Proof-oriented retrieval
goes through `search_for_proof`, and callers inspect one selected candidate by name when they need statement text or
declaration facts. The old synchronous index rebuild path remains out of the MCP surface: a model-controlled tool should
not be able to trigger a full environment walk plus bulk type rendering.

## Non-mutating proof actions

Proof actions are worker-backed overlays, not edits. `try_proof_step` reads one file, resolves a safe proof edit from
the cursor context, sends one capped candidate list to lean-rs, and reports per-candidate statuses plus bounded goals
and diagnostics. `verify_declaration` reads one file, targets one declaration by name or cursor, and reports policy
facts for diagnostics, unresolved goals, `sorry`/`admit`/`sorryAx`, and optional axioms.

The normal proof-agent loop is:

```text
proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration
```

Every step is bounded before crossing the worker boundary. Failed snippets, missing imports, unsupported shims, and
policy rejections are structured result statuses. `ServerError` remains reserved for infrastructure failures, and none
of these tools writes source files or creates a sandbox copy.

## The module-query cache

`proof_state` and `lean_query` call the worker's `process_module_query_batch` capability. There is no whole-file
info-tree result in the parent. `proof_state` hides the selector batch behind one proof-agent context with conservative
host-owned output budgets. `lean_query` keeps the selector model available for expert callers that need a custom
projection batch. The reference tools still use the older single-query capability until the later source/reference
redesign replaces them.

The worker owns module snapshot reuse for batched proof-agent queries and reports cache status, output bytes, and phase
timings in each batch outcome. The host deliberately does not keep an exact batch-result cache for `proof_state` or
`lean_query`; repeated calls reach the worker so warm hits, rebuilds, and evictions are observable. `cache.rs` still
ships a small LRU for the legacy single-query reference path, keyed on `(file_path, sha256(contents), query_shape)`.

The cache is populated through `LeanProject`'s worker actor. The tool reads the file, derives session imports from its
header, opens a short-lived worker session, passes the canonical file path as the worker `file_label`, and calls the
batch or single-query worker method. Files whose header references modules the session's open env does not have still
return a bounded result; the `MissingImports` signal travels as an envelope warning for single-file tools or a result
sidebar for `references_in_project`.

Frame size is controlled by query shape, not transport tuning. The project actor uses the upstream worker frame cap, and
the tool layer has no fallback that requests rendered `exprStr` / `typeStr` for every term in a file.
