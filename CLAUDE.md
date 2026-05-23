# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

MCP (Model Context Protocol) server that hosts Lean 4 **in-process** via `lean-rs`. The "host" in the name distinguishes
it from `lean-lsp-mcp`: this server owns a `LeanRuntime` + `LeanCapabilities` dylib directly rather than wrapping an
external LSP. Single crate, library + binary, stdio transport.

Reader-facing references live under `docs/`: `tool-catalog.md` is the per-tool request/result schema, `architecture.md`
is the deeper layering write-up. This file captures what's relevant only to working on the code itself.

Tools fall into four groups by shared plumbing: session-backed handlers (`src/tools/lean.rs`), the filesystem sweep
(`src/tools/scan.rs`), SQLite-indexed lookups (`src/tools/index.rs`), and the position-tool cluster
(`src/tools/position.rs`). The position tools drive `lean-rs` 0.1.4's `process_module_with_info_tree` (header-aware),
projecting the upstream `ProcessedFile` (four info-tree arrays plus diagnostics) through a content-hashed in-memory
cache (`src/cache.rs`). Three are cursor-driven (`goal_at_position`, `type_at_position`, `references_of_name`); the
fourth, `file_diagnostics`, is file-scoped but rides the same cache, so the typical agent loop ("what's wrong; then
probe the problem site") pays for the elaboration once. The shim is optional, so each position tool returns
`{ "status": "unsupported" }` cleanly when the loaded dylib lacks it. Missing imports become an envelope warning; header
parse failures short-circuit to a `header_parse_failed` status variant.

## Common commands

```sh
cargo build                                   # debug build
cargo build --release                         # release binary at target/release/lean-host-mcp
cargo clippy --all-targets -- -D warnings     # the lint gate (config is strict; see Cargo.toml [lints.clippy])
cargo test                                    # unit tests; no Lean fixture required
cargo test <name>                             # single test by name substring
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
    cargo test --test e2e -- --ignored        # opt-in E2E against a built Lake fixture
```

E2E tests also honor `LEAN_HOST_MCP_TEST_PACKAGE` / `LEAN_HOST_MCP_TEST_LIBRARY` (defaults `lean_rs_fixture` /
`LeanRsFixture`).

Running the server requires a Lake project that links the `lean-rs-host` shim (mandatory plus optional `lean_rs_host_*`
symbols; the exact symbol set is pinned by the `lean-rs` version in `Cargo.toml`). This crate does not bundle one; the
canonical template is `lean-rs/fixtures/lean/`. After `lake build` in that fixture, run:

```sh
./target/release/lean-host-mcp \
    --lake-root /path/to/lake/project \
    --package <pkg> --library <Lib> \
    --imports Your.Main.Module
```

All flags also read from `LEAN_HOST_MCP_{LAKE_ROOT,PACKAGE,LIBRARY,IMPORTS,CACHE_DIR}`.

## Architecture: the !Send lifetime tangle

This is the load-bearing constraint that shapes everything:

`lean-rs`'s `LeanRuntime`, `LeanHost`, `LeanCapabilities`, `LeanSession`, and `SessionPool` are all `!Send` and carry a
`'lean` lifetime anchored to a process-global runtime. They cannot cross a `tokio` task boundary, and they cannot live
inside an `Arc<Mutex<…>>` accessed from multiple async tasks.

`src/session.rs` solves this with an actor: `SessionHost::spawn` starts one dedicated OS thread that owns all Lean state
and serves a `tokio::mpsc::Receiver<Request>` in a blocking loop. Tool handlers submit a `Request` enum variant and
`await` a `oneshot` reply. From the caller side this looks like an async actor; the lifetime tangle stays hidden inside
the worker thread.

**Do not** try to:
- Hold a `LeanSession` across an `.await`.
- Wrap Lean state in `Arc`/`Mutex` and share it between tokio tasks.
- Return `LeanExpr` / `LeanName` from the worker thread: they're opaque handles. Project to owned strings/structs before
  sending over the oneshot.

The `SessionHost` API is intentionally closure- and channel-shaped so a subprocess-isolated backend (`lean-rs-worker`)
can replace the in-process implementation later without changes to the tools.

## Module layout

```
src/
  main.rs        clap CLI + rmcp stdio entry
  lib.rs         re-exports (LeanHostService, SessionHost, DeclarationIndex, …)
  server.rs      rmcp glue (LeanHostService)
  session.rs     SessionHost actor + projection structs (Diagnostic, ElabFailure, …)
  index.rs       DeclarationIndex: SQLite-backed deep module behind the three index tools
  cache.rs       ProcessedFileCache: LRU<Arc<ProcessedFile>> keyed on (path, sha256)
  envelope.rs    Response<T> = { result, freshness, warnings, next_actions }
  error.rs       ServerError
  tools/
    mod.rs       ToolContext (host + index + processed_files + lake_root + default_imports)
    lean.rs      six session-backed handlers
    scan.rs      project_scan: pure filesystem regex sweep, no Lean dependency
    index.rs     find_symbol / find_lemma / outline: thin wrappers over DeclarationIndex
    position.rs  goal_at_position / type_at_position / references_of_name: cache-backed
```

Tools are grouped by **shared plumbing**, not one-file-per-tool. The six `lean.rs` handlers all hit `SessionHost`; the
three `tools/index.rs` handlers all hit the SQLite index; the three `tools/position.rs` handlers share the
`ProcessedFileCache`; `scan.rs` is plumbing-free.

## The envelope contract

Every tool returns `Response<T>` from `envelope.rs`:

```jsonc
{ "result": { /* tool-specific */ },
  "freshness": { lake_root, imports, session_id, lean_toolchain },
  "warnings": [...],      // omitted when empty
  "next_actions": [...]   // omitted when empty
}
```

This is the **only** shape every tool shares. Three volatile decisions hide behind it: what "freshness" means, what a
warning looks like, and whether `next_actions` are present. Tools don't get to pick the shape; they fill it in.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors.
`ServerError` is only for infrastructure failures (worker thread gone, runtime init failed, Lake project unusable).

## build.rs note

`build.rs` bakes the Lean toolchain's `lib/lean` directory into the binary rpath so `libleanshared.{dylib,so}` loads at
runtime. `lean-rs-sys`'s build script does this for its own binaries but `cargo:rustc-link-arg` doesn't propagate, so
every crate that ships an executable loading Lean must repeat the dance. Discovery uses `$LEAN_SYSROOT` then falls back
to `lean --print-prefix`. macOS/Linux only.

## Lint posture

`Cargo.toml [lints.clippy]` is intentionally strict: read the table for the current set. Test files override with
file-level `#![allow(...)]`. When a warning is unavoidable in production code, add `#[allow(..., reason = "...")]` with
a concrete justification; `rg 'reason = ' src/` shows the established style.

## Version matrix

The supported `lean-rs` / Lean toolchain pairing lives in the README, which is the single source of truth. Bumping the
toolchain is a `lean-rs` change first, then a version bump here.
