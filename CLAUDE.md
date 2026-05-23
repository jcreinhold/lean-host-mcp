# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

MCP (Model Context Protocol) server that hosts Lean 4 **in-process** via `lean-rs`. The "host" in the name distinguishes it from `lean-lsp-mcp`: this server owns a `LeanRuntime` + `LeanCapabilities` dylib directly rather than wrapping an external LSP. Single crate, library + binary, stdio transport.

Status is v0.1: seven working tools (`elaborate`, `kernel_check`, `infer_type`, `whnf`, `is_def_eq`, `hover_by_name`, `project_scan`). Three more (`find_symbol`, `find_lemma`, `outline`) are deferred to v0.2 because published `lean-rs` 0.1.x lacks a `LeanName → String` rendering shim.

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

E2E tests also honor `LEAN_HOST_MCP_TEST_PACKAGE` / `LEAN_HOST_MCP_TEST_LIBRARY` (defaults `lean_rs_fixture` / `LeanRsFixture`).

Running the server requires a Lake project that links the `lean-rs-host` shim (exports 26 mandatory + 4 optional `lean_rs_host_*` symbols). v0.1 does not bundle one; the canonical template is `lean-rs/fixtures/lean/`. After `lake build` in that fixture, run:

```sh
./target/release/lean-host-mcp \
    --lake-root /path/to/lake/project \
    --package <pkg> --library <Lib> \
    --imports Your.Main.Module
```

All flags also read from `LEAN_HOST_MCP_{LAKE_ROOT,PACKAGE,LIBRARY,IMPORTS,CACHE_DIR}`.

## Architecture: the !Send lifetime tangle

This is the load-bearing constraint that shapes everything:

`lean-rs`'s `LeanRuntime`, `LeanHost`, `LeanCapabilities`, `LeanSession`, and `SessionPool` are all `!Send` and carry a `'lean` lifetime anchored to a process-global runtime. They cannot cross a `tokio` task boundary, and they cannot live inside an `Arc<Mutex<…>>` accessed from multiple async tasks.

`src/session.rs` solves this with an actor: `SessionHost::spawn` starts one dedicated OS thread that owns all Lean state and serves a `tokio::mpsc::Receiver<Request>` in a blocking loop. Tool handlers submit a `Request` enum variant and `await` a `oneshot` reply. From the caller side this looks like an async actor; the lifetime tangle stays hidden inside the worker thread.

**Do not** try to:
- Hold a `LeanSession` across an `.await`.
- Wrap Lean state in `Arc`/`Mutex` and share it between tokio tasks.
- Return `LeanExpr` / `LeanName` from the worker thread — they're opaque handles. Project to owned strings/structs before sending over the oneshot.

The `SessionHost` API is intentionally closure/channel-shaped so we can swap the in-process impl for `lean-rs-worker` (subprocess isolation) in v0.3 without touching tool code.

## Module layout

```
src/
  main.rs        clap CLI + rmcp stdio entry
  lib.rs         re-exports (LeanHostService, SessionHost, Response, …)
  server.rs      rmcp glue (LeanHostService)
  session.rs     SessionHost actor + projection structs (Diagnostic, ElabFailure, …)
  envelope.rs    Response<T> = { result, freshness, warnings, next_actions }
  error.rs       ServerError
  tools/
    mod.rs       ToolContext (shared host + lake_root + default_imports)
    lean.rs      six session-backed handlers
    scan.rs      project_scan — pure filesystem regex sweep, no Lean dependency
```

Tools are grouped by **shared plumbing**, not one-file-per-tool. The six `lean.rs` handlers all hit `SessionHost`; `scan.rs` is plumbing-free.

## The envelope contract

Every tool returns `Response<T>` from `envelope.rs`:

```jsonc
{ "result": { /* tool-specific */ },
  "freshness": { lake_root, imports, session_id, lean_toolchain },
  "warnings": [...],      // omitted when empty
  "next_actions": [...]   // omitted when empty
}
```

This is the **only** shape every tool shares. Three volatile decisions hide behind it: what "freshness" means, what a warning looks like, and whether `next_actions` are present. Tools don't get to pick the shape — they fill it in.

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors. `ServerError` is only for infrastructure failures (worker thread gone, runtime init failed, Lake project unusable).

## build.rs note

`build.rs` bakes the Lean toolchain's `lib/lean` directory into the binary rpath so `libleanshared.{dylib,so}` loads at runtime. `lean-rs-sys`'s build script does this for its own binaries but `cargo:rustc-link-arg` doesn't propagate, so every crate that ships an executable loading Lean must repeat the dance. Discovery uses `$LEAN_SYSROOT` then falls back to `lean --print-prefix`. macOS/Linux only.

## Lint posture

`Cargo.toml [lints.clippy]` is intentionally strict (ported from `lean-dup`): `pedantic`, `nursery`, `cargo` all at `warn`, plus `unwrap_used`/`expect_used`/`panic`/`indexing_slicing`/`arithmetic_side_effects` at `warn`, `unsafe-code` at `deny`. Test files override these with file-level `#![allow(...)]`. When a warning is unavoidable in production code, add a `#[allow(..., reason = "...")]` with a concrete justification — see `main.rs:60-63` and `session.rs:26` for the established style.

## Version matrix

| `lean-host-mcp` | `lean-rs` / `lean-rs-host` | Lean toolchain |
| --- | --- | --- |
| 0.1.x | 0.1.x | leanprover/lean4 v4.29.x (pinned by `lean-rs`) |

Bumping the supported Lean toolchain is a `lean-rs` change first, then a crate-version bump here.
