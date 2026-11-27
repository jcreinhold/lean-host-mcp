# lean-host-mcp

MCP server hosting Lean 4 in supervised worker children through `lean-rs-worker-parent` and `lean-rs-worker-child`.
Calls reach Lean's elaborator and kernel directly. A server serves one Lake project, and that project's toolchain
selects its worker.

Read [README.md](README.md) for setup and the version matrix, [docs/tool-catalog.md](docs/tool-catalog.md) for public
schemas, [docs/operations.md](docs/operations.md) for configuration and troubleshooting, and
[docs/architecture.md](docs/architecture.md) for implementation details.

## Build and validation

Build each member separately. `cargo build --workspace` unifies `lean-rs-sys` features and can link `libleanshared` into
the parent, breaking multi-toolchain dispatch. Workspace clippy does not link and is safe.

```sh
cargo build --locked -p lean-host-mcp
cargo build --locked -p lean-host-mcp-worker
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p lean-host-mcp
cargo test --locked -p lean-host-mcp --lib
cargo test --locked -p lean-host-mcp-worker --all-targets
scripts/test-installed-stdio.sh --all-targets
cargo test --locked -p lean-host-mcp --test http_transport
```

The parent library tests need no Lean fixture. The all-target runner provisions a disposable installed worker for the
stdio EOF check. Opt-in e2e and build tests require built fixture `.olean` files and a matching worker:

```sh
LEAN_HOST_MCP_WORKERS_DIR=$PWD/target/release \
LEAN_HOST_MCP_TEST_FIXTURE=$PWD/fixtures/lean \
    cargo test --locked -p lean-host-mcp --test e2e --test build_requests --test artifact_invalidation \
    -- --ignored --test-threads=1
```

Run these tests one at a time: they start Lean processes, and the timing tests assume no other load. On macOS this
command also cannot reach a worker: Cargo sets `DYLD_FALLBACK_LIBRARY_PATH` for test binaries, and the worker refuses
any `DYLD_*` variable. Run the binaries that `cargo test --no-run` prints, with the same arguments, under
`env -u DYLD_FALLBACK_LIBRARY_PATH` instead; see `docs/operations.md`.

For a non-default toolchain, export `LEAN_SYSROOT=${ELAN_HOME:-$HOME/.elan}/toolchains/leanprover--lean4---<id>` for the
whole build-and-test invocation. Set `LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>` for the worker build too. The latter chooses
its rpath; it does not select the header fingerprint baked into dependencies and parent tests. A mismatched header fails
bootstrap with `lean_rs.loader.unsupported_toolchain_fingerprint`. See CI's environment setup.

Use `scripts/check-parent-link.sh` to build and inspect the parent separately in release mode.

Run targeted checks for the changed behavior. Before tagging, run the full `scripts/prerelease.sh`; `--quick` skips
cargo-deny and is for iteration. Release and toolchain-bump procedures live in `.claude/skills/`, with discovery aliases
under `.agents/skills/`.

## Release rule

Version by semver. While the version is 0.x, a breaking change to the tools, the configuration, the CLI or the library
bumps the minor version; any other change bumps the patch version.

Test changes spanning the SDK and this repo together locally before pushing or tagging. Use temporary Cargo
`[patch.crates-io]` overrides; both installed-worker scripts take `LEAN_RS_CHECKOUT=<SDK checkout>` and
`LEAN_SEMANTIC_SEARCH_CHECKOUT=<semantic search checkout>` and build the worker and host from them, and the worker
brings the SDK's shim sources. Check the resolved crates with `cargo metadata --format-version 1`, then run the affected
real Lean operation and this repo's required `scripts/prerelease.sh` checks. Do not publish an SDK crate just to test it
here. Do not commit local paths or an override lockfile. When the whole change passes locally, release the batch
upstream first, resolve the published pins, and keep the required release checks.

## Runtime boundaries

Before adding a check, record, lock, or scheduler, write one sentence naming the requirement it enforces and the native
tool that already owns that work: Lake for builds, the kernel for locks and process exit, SQLite for atomic writes,
Cargo for dependency overrides. If the native tool can enforce the requirement with a flag or one call, use that.
Otherwise add one check at the one point where the requirement can fail, and delete it when the requirement goes.

- One server owns a checkout. `server.lock` admits one server per store, and every process the server starts inherits
  `children.lock`, so the next server serves only after those processes exit (`store.rs`, `children.rs`). Never unlock
  either lock, and never signal a process group whose child file is free.
- Builds go through `lean_build`: the root's write turn in `project_lock.rs`, then each step as `lake <args>` in its own
  process group. Lake decides what to build and in which order; the server checks only the words it hands to Lake. A
  test fails if the build path queries Lake's import graph. A raw `lake build` is outside the contract.
- Only `crates/lean-host-mcp-worker` links `libleanshared`. The parent uses the link-free worker-parent/protocol and
  toolchain APIs; Lean runtime types and opaque handles stay in the child. The parent's `build.rs` embeds Git identity
  only; the worker's `build.rs` emits the Lean rpath.
- `LeanProject` owns a private serialized controller on a dedicated OS thread. Its bounded mailbox carries typed
  `ProjectMessage` requests with reply channels. Keep worker handles owned by that controller, and keep borrowed
  `LeanWorkerSession<'_>` values inside their owning stack frame. Do not hold a session across `.await` or share a
  handle between tokio tasks through `Arc<Mutex<_>>`.
- Route worker calls through the controller's common deadline, cancellation, lifecycle, and retry handling. Preserve
  admission deadlines through dispatch. Do not retry requests whose execution may already have started merely because
  the caller lost its result.
- The broker can pool several actors per project within the global worker cap. Keep capacity accounting tied to actual
  worker retirement, including failed startup and shutdown paths.
- Keep HTTP/axum wiring in the binary transport module. `LeanHostService`, `ProjectBroker`, and tools remain
  transport-agnostic. HTTP binds are loopback-only; the server has no built-in authentication or TLS.
- Stdout is JSON-RPC while serving stdio. Use `tracing` for server diagnostics; CLI subcommands may print their output.
  Lean commands run with server privileges, without an OS sandbox.

## Memory and imported sessions

`project/runtime_config.rs` sets memory policy and `project.rs` applies it. Preserve these distinctions when changing
either worker builder:

- RSS is telemetry only: shared, clean, mapped `.olean` pages make it unsuitable for restart, recycle, or refusal
  thresholds. Do not add RSS or request-count restart limits.
- `max_import_residue_bytes` bounds retained imports using `non_memory_mapped_region_bytes`. Import count is not a
  sizing unit; `WORKER_MAX_IMPORTS_BACKSTOP` only guards unusable zero-valued import statistics. Keep the supervisor's
  all-causes restart intensity above normal planned cycling and retain the actor's abnormal-restart breaker.
- The Lean heap ceiling must exceed healthy import work. `lean_max_memory_kib_for` derives it from the residue budget
  plus `max(budget, 8 GiB)` headroom. Allocator exhaustion can throw a C++ exception through Rust and abort the child
  (status 134); it is not reliably an ordinary Lean-domain result. Do not replace the derived default with a fixed cap.
- Session-pool capacity and the residue budget are independent. Keep warm profiles when imports change. A build stops
  every worker before Lake runs, so no build changes what a live worker's profiles loaded; the finished-build count is a
  backstop that cycles the whole worker. Couple reuse hints and `imports_seen` to pool capacity, not the import-count
  backstop.
- Above the soft residue threshold, prefer the quiet-period recycle and pre-warm path. Preserve its guard against
  repeated cycling without intervening work. Measured costs and sizing evidence belong in `docs/architecture.md`.

## Tool and response contracts

The public proof surface is `lean_context`, `lean_trial`, `lean_verify`, `lean_lookup`, and `lean_status`.
`lean_runtime` reports worker activity and `lean_compatibility` checks a worker's compatibility; `lean_run` and
`lean_get` start and read named requests, whose rows live in the SQLite table of `requests/`. `lean_build` runs Lake
under the project's write turn, and `lean_imports` reads each file's import closure from Lake. `server.rs` owns
registration; `tools/semantic.rs` adapts internal operations to public `{ data, errors, trust }` responses. The internal
`Response<T>` in `envelope.rs` is not the public schema. Consult the catalog before changing either layer; keep field
semantics and failure distinctions intact.

Lean parse, elaboration, kernel, and meta failures are domain data. Infrastructure failures use `ServerError` or
structured runtime-unavailable results. Optional proof-position support must degrade cleanly when the loaded shim lacks
it. Keep project identity, artifact freshness, execution disposition, and proof verdict separate. Request rows,
`lean_build` and `lean_imports` have their own shapes; do not force them into the semantic envelope.

## Maintenance

The root manifest declares coupled lean-rs and lean-semantic-search versions once. The supported window comes from
`lean_toolchain::SUPPORTED_TOOLCHAINS`; widening it starts upstream. Keep the README version matrix current.

`config_schema.rs` owns the generated configuration table in `docs/operations.md` and `config init` output. Update the
source and regenerate the marked table together when configuration changes.

Clippy is intentionally strict. Use a concrete `reason = "..."` for unavoidable production-code allowances. Local Claude
PostToolUse hooks provide best-effort formatting and non-blocking contract reminders; they are not validation gates. The
architecture reviewer is read-only and checks the boundaries above.

## Storage and evidence limits

- Keep no evidence by default. When a result changes a release or maintainer decision, keep one short Git-tracked
  summary: revision, exact check, outcome, and first error needed to act. Do not retain raw logs or traces.
- Before any task writes outputs, name the canonical paths, total byte limit, owner, next named check, and expiry. Keep
  task outputs within this checkout. Do not use `~/Documents/Codex`, `/private/tmp` (including `/tmp`), or `.git` for
  non-Git data. Do not evade limits with alternate paths, aliases, archives, compression, or split files.
- Use installed Elan toolchains and one normal build cache. Do not copy toolchains, targets, or checkouts as evidence.
  If a check cannot meet these limits, stop and report the conflict rather than create another storage location.
- Delete generated task outputs when no next named check owns them, after confirming no live work needs them. Preserve
  active inputs and unique source. Measure allocated bytes before and after cleanup; report retained paths, bytes,
  owners, and expiry. A command succeeding is not proof that cleanup happened.
- Prove Git blob checks with small passing and oversized failing cases, including renamed and partially staged files,
  before claiming enforcement. If the gate is absent or untested, say so. Git checks do not bound ignored or untracked
  output: enforce and measure those limits at the writer. These rules do not authorize a new storage service.
