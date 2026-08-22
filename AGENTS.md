# lean-host-mcp

## What this is

MCP (Model Context Protocol) server that hosts Lean 4 in a supervised **worker child** via `lean-rs-worker-parent` +
`lean-rs-worker-child`. The "host" in the name distinguishes it from `lean-lsp-mcp`: the parent owns a shims-only
`LeanWorkerHostHandle`, the worker child owns the `LeanRuntime` + bundled host-shim capabilities, and calls reach Lean's
elaborator and kernel directly rather than going through an external LSP. A wedged tactic, typeclass loop, or OOM kills
the *child*, which the supervisor restarts.

Two-member Cargo workspace: `crates/lean-host-mcp/` (library + parent binary; does **not** link `libleanshared`) and
`crates/lean-host-mcp-worker/` (the per-toolchain worker binary; the only crate that links `libleanshared`). The split
is deliberate—see "Multi-toolchain dispatch" below.

Reader-facing references live under `docs/`: `tool-catalog.md` is the per-tool request/result schema, `architecture.md`
is the deeper layering write-up. This file captures what's relevant only to working on the code itself.

The public surface is a six-tool proof workflow, grouped in `src/tools/` by shared plumbing:
`proof_state -> search_for_proof -> inspect_declaration -> try_proof_step -> verify_declaration`, with `find_references`
as the semantic-lookup companion. `declaration.rs` backs `inspect_declaration`; `proof_search.rs` backs
`search_for_proof`; `proof_action.rs` backs `try_proof_step` and `verify_declaration`; `position.rs` backs `proof_state`
and `find_references`; `source_input.rs` is the shared source-reading helper. `proof_state` depends on an optional
bounded shim and returns `{ "status": "unsupported" }` cleanly when the loaded dylib lacks it. Lean-domain failures
(parse, elaboration, kernel rejection, meta timeout) are part of the `ok` payload; missing imports become an envelope
warning.

## Common commands

```sh
cargo build -p lean-host-mcp                            # debug build, parent only
cargo build -p lean-host-mcp-worker                     # debug build, worker only
cargo build --release -p lean-host-mcp                  # release parent (no libleanshared link)
cargo build --release -p lean-host-mcp-worker           # release worker (links libleanshared)
cargo clippy --workspace --all-targets -- -D warnings   # lints; --workspace is safe (clippy doesn't link)
cargo test -p lean-host-mcp                             # parent tests (no Lean fixture required)
cargo test -p lean-host-mcp <name>                      # single test by name substring
cargo test -p lean-host-mcp --test http_transport       # black-box Streamable HTTP transport tests
LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
    cargo test -p lean-host-mcp --test e2e -- --ignored   # opt-in end-to-end against a real fixture
cargo bench -p lean-host-mcp --bench worker_roundtrip     # gated on LEAN_HOST_MCP_BENCH_FIXTURE
```

### Always build per-member, never `--workspace`

`cargo build --workspace` unifies the `lean-rs-sys` feature set across the parent and worker crates, silently re-linking
`libleanshared` into the parent binary. The whole multi-toolchain story depends on the parent staying free of
`libleanshared`, so always use `-p <member>`. The link-set assertion below catches regressions:

```sh
! otool -L target/release/lean-host-mcp | grep -q libleanshared    # macOS
! ldd  target/release/lean-host-mcp | grep -q libleanshared        # Linux
```

`cargo clippy --workspace` is *safe* (no linking happens). Workspace-wide `cargo test` is correctness-safe but masks the
link-set invariant; keep link assertions on `-p`-scoped release builds.

### Running from a development checkout

The parent resolves the worker binary from `~/.local/share/lean-host-mcp/workers/<toolchain>/lean-host-mcp-worker`,
populated by `lean-host-mcp install-worker`. During development you can shortcut this with `LEAN_HOST_MCP_WORKERS_DIR`:

```sh
cargo build --release -p lean-host-mcp-worker
LEAN_HOST_MCP_WORKERS_DIR=$PWD/target/release \
    cargo run --release -p lean-host-mcp -- ...
```

When `LEAN_HOST_MCP_WORKERS_DIR` is set, resolution looks for `<dir>/lean-host-mcp-worker` first (bare developer layout
matching `target/release/`), then `<dir>/<toolchain>/lean-host-mcp-worker`.

E2E tests also honor `LEAN_HOST_MCP_TEST_PACKAGE` / `LEAN_HOST_MCP_TEST_LIBRARY` (defaults `lean_rs_fixture` /
`LeanRsFixture`).

When exercising a non-default toolchain locally (e2e, or a worker built by hand rather than via `install-worker`),
export `LEAN_SYSROOT=$HOME/.elan/toolchains/leanprover--lean4---<id>` for the *whole* build-and-test invocation, the
way CI does. The `lean-toolchain` crate's build script bakes the discovered toolchain's `lean.h` digest into every
binary it is linked into — parent test binaries included — and the semantic-capability preflight compares that baked
digest against the per-toolchain capability manifest. Building with only `LEAN_HOST_MCP_TARGET_TOOLCHAIN` set updates
the worker's rpath but leaves the bindings (and the parent's baked digest) on whatever `lean --print-prefix` resolves
to, so a worker built that way for a non-default toolchain fails bootstrap with
`lean_rs.loader.unsupported_toolchain_fingerprint`.

Running the server requires any Lake project whose requested imports have built `.olean` files. The 34 mandatory + 13
optional `lean_rs_host_*` symbols come from the vendored shim Lake package inside `lean-rs-host`
(`crates/lean-rs-host/shims/lean-rs-host-shims/`), which the host builds once per toolchain and loads without building
the consumer's `:shared` facet—consumers don't declare or link it. Each project's `lean-toolchain` pin selects the
worker binary; install one first with `lean-host-mcp install-worker --toolchain <id>` (or plain
`lean-host-mcp install-worker` to scan `~/.elan/toolchains`). The workspace's `fixtures/lean/` is a demo target the test
suite uses; it isn't a template consumers must mirror. Then point the server at a project:

```sh
./target/release/lean-host-mcp --lake-root /path/to/lake/project
```

`LEAN_HOST_MCP_PROJECT` provides the corresponding project override.

Stdio is the default transport. `--bind 127.0.0.1:PORT` (or `LEAN_HOST_MCP_BIND`) selects Streamable HTTP instead, with
`/mcp` as the default route. `--http-path` only applies with `--bind`; non-loopback binds are rejected because the HTTP
server has no built-in authentication or TLS.

```sh
cargo run -p lean-host-mcp -- serve --lake-root /path/to/lake/project --bind 127.0.0.1:8765
```

The parent crate enables rmcp's `transport-streamable-http-server` feature and hosts it through axum. Keep that HTTP
wiring inside the binary transport module; `LeanHostService`, `ProjectBroker`, and tools must remain transport-agnostic.

## Architecture: closure-channel actor over a worker child

The parent process never sees `lean-rs` or `lean-rs-host` types directly—those live inside the worker child
(`crates/lean-host-mcp-worker/src/main.rs`, a 2-line entry that calls `lean_rs_worker_child::run_worker_child_stdio()`).
The parent owns a `LeanWorkerHostHandle` (re-exported from `lean-rs-worker-parent`) and short-lived
`LeanWorkerSession<'_>` borrows that don't escape their owning stack frame.

## Multi-toolchain dispatch

`crates/lean-host-mcp/src/toolchain.rs` resolves a Lake project's `lean-toolchain` pin to a per-toolchain worker binary
under `~/.local/share/lean-host-mcp/workers/<id>/lean-host-mcp-worker`. `LeanProject::open` calls
`WorkerBinary::resolve_for` up front; a missing binary surfaces as `ServerError::BadProject` whose message includes the
exact `lean-host-mcp install-worker --toolchain <id>` command to fix it. Each worker binary is built with
`LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>` so the worker crate's `build.rs` bakes the matching `lib/lean` directory into its
rpath.

Each spawned worker inherits its `LEAN_SYSROOT` from the per-toolchain `LeanWorkerChild::for_toolchain(path, sysroot)`
binding in `crates/lean-host-mcp/src/project.rs`. The sysroot comes from `ToolchainId::elan_dir()`—the same root that
produced the worker binary's rpath. **A single server process can host workers for multiple toolchains**: each project
resolves its own pinned toolchain, the parent spawns one worker per toolchain, and the supervisor sets `LEAN_SYSROOT`
invisibly per spawn. The MCP client does not need to set `LEAN_SYSROOT` in the server's `env` block.

`LeanProject` (`src/project.rs`) is the actor. It parks the handle on a dedicated OS thread—one per project—so "exactly
one owner of the Lean runtime at a time" is structural, not a lock discipline. The channel carries a closure type, not a
Request enum:

```rust
type Job = Box<dyn FnOnce(&mut LeanWorkerHostHandle) + Send + 'static>;
// dispatch loop, on the dedicated thread:
while let Some(job) = rx.blocking_recv() { job(&mut handle); }
```

Each public method on `LeanProject` enqueues one closure that opens a worker session with the requested imports, calls
the typed worker method, projects the worker's wire-stable result into the MCP-stable wire shape, and replies via
`oneshot`. Adding a tool is one method on `LeanProject` plus maybe one projection helper—no `Request` variant +
state-machine arm + `do_*` method to coordinate.

**Do not** try to:
- Hold a `LeanWorkerSession<'_>` across an `.await` (it borrows from `&mut LeanWorkerHostHandle`).
- Wrap `LeanWorkerHostHandle` in `Arc`/`Mutex` and share it between tokio tasks.
- Add a `Request` enum back. The closure-channel shape is deliberately the simpler form.
- Restart, recycle, or refuse a call on **resident** memory. RSS counts the shared, clean, mmapped `.olean` pages every
  Mathlib-scale worker maps at startup, so any RSS threshold fires on healthy workers. Four of them used to live here;
  they existed to contain a child that re-imported on every call, which is now fixed upstream (a session whose imports
  match the live one is reused, so retained import state no longer scales with call count). What replaced them is
  `runtime.lean_max_memory_kib`, a Lean **heap** ceiling enforced inside the child. RSS is sampled once per call for the
  `rss_kib` fact and read by no policy.
- Give that heap ceiling a fixed value, and do not believe the old claim that its overrun is ordinary Lean-domain data
  in the `ok` payload — it is not, and the difference is a dead child. Lean's allocator throws a C++ exception, which
  cannot unwind through the Rust frame at the shim FFI boundary: `fatal runtime error: Rust cannot catch foreign
  exceptions, aborting`, exit status 134. (Lean's *monadic* check point does raise a catchable error, but
  `importModules` never reaches it.) So the ceiling must sit above everything healthy work reaches, and healthy work
  reaches the residue budget by construction. A fixed 8 GiB under a 9 GiB budget floor meant the abort always preempted
  the clean recycle and the residue policy was unreachable at Mathlib scale — a three-file kan-proofs sweep aborted the
  child on file three. `lean_max_memory_kib_for` derives it as twice the budget, so the order is structural.
- Add `max_requests`, an RSS ceiling, or an import *count* ceiling to either worker builder. The **one** policy restart
  both builders set is `max_import_residue_bytes`, because imports are the one thing a child accumulates without bound:
  an environment imported with `loadExts := true` is never reclaimed, so retained state scales with what a child has
  imported even though session reuse removed the scaling with *call* count. It is denominated in bytes because a count
  is the wrong unit — measured on real per-file headers at Mathlib scale, the same *four* imports cost 9.6 GiB on one
  workload and 16.0 GiB on another, and only the first import in a process is memory-mapped (10–18 MB; every later one
  re-materializes its whole closure at 2.0–4.5 GB, overlap included). The parent accumulates
  `LeanWorkerImportStats::non_memory_mapped_region_bytes`, which predicts Δ`phys_footprint` with slope 1.09 / R² 0.96;
  `compacted_region_bytes`, extension entries, and module count all fail the ≥0.9 bar. `WORKER_MAX_IMPORTS_BACKSTOP`
  (32) stays only as the guard against a child that reports zero-valued import stats — that path advances
  `import_stats_unusable`. `SUPERVISOR_RESTART_INTENSITY` raises the supervisor's own all-causes backstop above the
  planned cycle rate — its default of 16/min is terminal when exhausted. The crash-loop breaker
  (`MAX_RESTARTS_PER_WINDOW`, abnormal causes only) and the per-request deadline are this actor's own and stay.
- Drop the outgoing session when the imports change, or re-couple the pool to the restart bound. Since the residue is
  unreclaimable either way, the child *pools* imported sessions — capacity `LEAN_RS_WORKER_SESSION_POOL_CAPACITY`, set
  from the first-class `LeanWorkerConfig::session_pool_capacity` (`WORKER_SESSION_POOL_CAPACITY = 8`). It used to be
  derived from `max_imports`, which made the child's LRU eviction dead code: the parent cycled the child exactly where
  the pool would have started evicting. A profile the pool still holds is restored by key comparison, not by importing,
  at a measured ~70 MiB per extra live environment against 2.0–4.5 GB per import. (The pool can also evict below
  capacity, when an import registers a Lean environment extension and leaves a held environment permanently
  short-`extensions`; that direction only frees slots.) The residue budget and the pool capacity are now independent:
  the budget bounds bytes per child generation, the capacity bounds how many warm profiles a generation holds, and
  `reuse_hint_capacity` plus `ProjectActorState::imports_seen` both track the *capacity* — the latter so
  `cycle_if_imports_rebuilt` still catches a `.olean` rebuilt under a profile that another profile ran in between.
- Take the residue cycle on the request path when it can be taken off it. Above 60% of the budget the actor's receive
  loop waits with a `QUIESCENCE_GRACE` timeout instead of `blocking_recv`, and on a quiet tick it cycles and re-imports
  the most recent profile (`RestartCause::ImportResidueIdle`). The `import_residue_idle : import_residue` ratio is this
  design's KPI. Under the soft budget, or with no tokio handle (sync tests), the timeout is not armed and the loop is
  the plain blocking receive it always was.

Lean-domain failures cross the worker boundary as `Serialize + Deserialize` data (`LeanWorkerElabFailure`,
`LeanWorkerMetaResult<T>`, etc.); the projection from worker types to MCP types is pure Rust data shuffling, with no
opaque handles to manage.

## Module layout

```
crates/
  lean-host-mcp/                # parent crate (no libleanshared link)
    src/
      main.rs           clap CLI: `serve` (default) + `install-worker`
      lib.rs            re-exports (LeanHostService, ProjectBroker, ToolchainId, …)
      server.rs         rmcp glue (LeanHostService)
      transport_http.rs axum/rmcp Streamable HTTP wiring
      broker.rs         ProjectBroker: per-project LRU pool, idle reaper, hint resolution
      project.rs        LeanProject closure-channel actor + worker resolution
      toolchain.rs      ToolchainId / WorkerBinary / ToolchainError
      cli/install_worker.rs   `install-worker` subcommand
      projections.rs    worker-type -> MCP-wire projection helpers
      cache.rs          ModuleQueryCache: bounded module-query result cache
      envelope.rs       Response<T> = { status, result, freshness, telemetry?, warnings, next_actions }
      config_file.rs    on-disk [runtime]/[broker]/[server]/[telemetry]/[output] config
      config_schema.rs  SCHEMA_FIELDS: `config init` output + the generated docs/operations.md table
      error.rs          ServerError
      lake_meta.rs      LakeProjectMeta + lakefile discovery
      diagnosis.rs      execution-taint classification for verdicts computed under duress
      trust.rs          public `trust` block: project identity + artifact freshness facts
      ilean.rs          `.ilean` reference reads for project-scope find_references
      semantic_search.rs  in-call ranking over declaration rows
      smoke.rs          shared harness bits for the black-box integration tests
      tools/
        mod.rs          ToolContext + shared tool plumbing
        semantic.rs     public facade: the five `lean_*` tools and their `kind` dispatch
        declaration.rs  inspect_declaration
        declaration_inventory.rs  module/file declaration outlines
        proof_search.rs search_for_proof
        proof_action.rs try_proof_step / verify_declaration
        position.rs     proof_state / find_references
        changed_coverage.rs  git-diff-scoped verification targets
        source_input.rs shared source-reading helper
  lean-host-mcp-worker/         # worker child binary (only crate that links libleanshared)
    build.rs            emits rpath; honors LEAN_HOST_MCP_TARGET_TOOLCHAIN
    src/main.rs         2-line entry: lean_rs_worker_child::run_worker_child_stdio()
```

Tools are grouped by **shared plumbing**, not one-file-per-tool: the handlers in each `tools/` file share the worker
call and projection path for that stage of the proof workflow.

## The envelope contract

Every tool returns `Response<T>` from `envelope.rs`:

```jsonc
{ "status": "ok",                 // or "runtime_unavailable"
  "result": { /* tool-specific; null on runtime_unavailable */ },
  "runtime_error": null,          // populated on runtime_unavailable
  "freshness": { project_root, session_id, lean_toolchain },  // always emitted; small, stable identity
  "telemetry": { project_hash, imports, runtime },            // omitted unless telemetry.verbosity = full
  "warnings": [...],              // omitted when empty
  "next_actions": [...]           // omitted when empty
}
```

This is the **only** shape every tool shares. Volatile decisions hide behind it: what "freshness" means, what a warning
looks like, whether `next_actions` are present. Tools don't pick the shape; they fill it in. The full field-by-field
contract lives in `docs/tool-catalog.md` and the README.

`envelope.rs` splits a producer's `Freshness` snapshot into the always-serialized `FreshnessIdentity`
(`project_root`/`session_id`/`lean_toolchain`) and a `Telemetry` block (`project_hash`, the full `imports` list, and the
worker `RuntimeFacts`). `finalize` in `server.rs` drops `telemetry` entirely under the default `telemetry.verbosity =
quiet`, because none of it helps an agent make a proof step — the one actionable signal a worker restart carries already
surfaces as a top-level `warning`. Set `telemetry.verbosity = full` to re-inline it. Two presentation knobs ride on
`ToolConfig` (resolved once at startup from `[server]`/`[telemetry]`/`[output]` config): `server.response_carrier`
(`text` default / `structured` / `both`) chooses whether the JSON envelope rides in `content` text, `structuredContent`,
or both — handlers return a bare `CallToolResult`, so rmcp advertises **no** `outputSchema` (the Anthropic Messages API
drops it anyway, and deep `$defs` break strict clients).

Lean-domain failures (parse, elaboration, kernel rejection, meta timeout) are part of the `Ok` payload, not MCP errors.
`ServerError` is only for infrastructure failures (worker thread gone, runtime init failed, Lake project unusable).

## build.rs note

`crates/lean-host-mcp-worker/build.rs` bakes the Lean toolchain's `lib/lean` directory into the worker binary's rpath so
`libleanshared.{dylib,so}` loads at runtime. The parent crate has **no** `build.rs`; it does not link `libleanshared`.
Discovery order: `LEAN_HOST_MCP_TARGET_TOOLCHAIN` (resolved as `~/.elan/toolchains/leanprover--lean4---<id>`), then
`LEAN_SYSROOT`, then `lean --print-prefix`. macOS/Linux only.

## Lint posture

`Cargo.toml [lints.clippy]` is intentionally strict: read the table for the current set. Test files override with
file-level `#![allow(...)]`. When a warning is unavoidable in production code, add `#[allow(..., reason = "...")]` with
a concrete justification; `rg 'reason = ' src/` shows the established style.

## Local automation

Two PostToolUse hooks run on every edit (`.claude/settings.json`): `.claude/hooks/format.sh` reformats the touched file
(rustfmt / taplo / mdwright, best-effort, never blocks), and `.claude/hooks/contract-guard.sh` posts non-blocking
reminders for the two invariants greps can catch — no `println!`/`print!` in `src/` (stdout is the stdio transport), and
no Lean-runtime dep creeping into the parent manifest. The `architecture-reviewer` agent covers the deeper invariants
(closure-channel actor, envelope contract, transport-agnostic core).

`scripts/prerelease.sh` is the local CI mirror plus the gates CI omits but the repo already configures: the parent ⊥
libleanshared release link-set assertion, `taplo`/`mdwright`/`prettier` format checks, `cargo deny`, and `cargo shear`.
Run it before tagging (`--quick` skips cargo-deny for iteration). The `/release-lean-host-mcp` skill walks the full
release checklist.

## Version matrix

The supported `lean-rs` / Lean toolchain pairing lives in the README, which is the single source of truth. Bumping the
toolchain is a `lean-rs` change first, then a version bump here.
