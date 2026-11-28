# lean-host-mcp

An MCP server that runs one Lake project's Lean elaborator and kernel in supervised worker children through lean-rs. The
[README](README.md) covers setup and versions, the [tool catalog](docs/tool-catalog.md) the public contract,
[operations](docs/operations.md) running the server, and [architecture](docs/architecture.md) the requirements, the
owners and each invariant with its test.

## Build and test

Build each member with `-p`: `cargo build --workspace` unifies the `lean-rs-sys` features and links `libleanshared` into
the parent. Clippy links nothing, so `--workspace` is safe there.

```sh
cargo build --locked -p lean-host-mcp
cargo build --locked -p lean-host-mcp-worker
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p lean-host-mcp
cargo test --locked -p lean-host-mcp --lib
cargo test --locked -p lean-host-mcp-worker --all-targets
scripts/test-installed-stdio.sh --all-targets  # an installed worker's EOF and exit
scripts/test-installed-module-setup.sh         # the packaged pair and proof search
scripts/check-parent-link.sh                   # build the release parent and inspect its link set
CARGO_NET_OFFLINE=true cargo test --locked -p lean-host-mcp --test http_transport \
    install_and_serve_keep_no_state_in_an_empty_home -- --ignored
```

The library tests need no Lean. The ignored tests start Lean: they need the fixture built and a worker that
`install-worker` installed for its toolchain, since the worker Cargo builds refuses to serve without the files install
puts beside it. Run them one at a time, each with a new store; the timing tests in `tests/requests.rs` assume no other
load, so rerun a failed one alone before you treat it as a fault.

On macOS, Cargo sets `DYLD_FALLBACK_LIBRARY_PATH` for the test binaries it runs, and the worker refuses any `DYLD_*`
variable. Build the ignored suites with `cargo test --no-run`, point `LEAN_HOST_MCP_WORKERS_DIR` at an installed worker,
keep `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` and `TMPDIR` under `target/`, and run each test of each printed `BINARY` alone:

```sh
for name in $(BINARY --list --ignored | sed -n 's/: test$//p'); do
    env -u DYLD_FALLBACK_LIBRARY_PATH LEAN_HOST_MCP_TEST_FIXTURE="$PWD/fixtures/lean" \
        LEAN_HOST_MCP_STORE_DIR="$(mktemp -d "$PWD/target/store.XXXXXX")" \
        BINARY --ignored --exact "$name"
done
```

A test that ranks proofs needs the search runtime in that cache first; `examples/prepare_semantic_smoke.rs` builds it.
The empty-`HOME` test clears its children's environment, so it runs under `cargo test`.

For another toolchain, export `LEAN_SYSROOT=${ELAN_HOME:-$HOME/.elan}/toolchains/leanprover--lean4---<id>` for the whole
build and test run, and `LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>` for the worker build, which sets its rpath. A header that
does not match fails with `lean_rs.loader.unsupported_toolchain_fingerprint`. CI's environment setup does both.

The installed smoke scripts need Python 3.11 or later and a native build, and resolve `fixtures/lean/lean-toolchain`
through elan. Lint the workflows with `actionlint` and the scripts with `shellcheck scripts/*.sh`. Before a tag, run
`scripts/prerelease.sh` in full; `--quick` skips cargo-deny.

Two harnesses measure. `tests/smoke_perf.rs` (`black_box_mcp_smoke_perf_baseline`, ignored) prints JSONL rows of time,
bytes and worker facts for a fixed set of calls; `LEAN_HOST_MCP_SMOKE_PROJECT`, `LEAN_HOST_MCP_SMOKE_FILE` and
`LEAN_HOST_MCP_SMOKE_DECLARATION` point it at another project. Keep its output with a performance change.
`uv run scripts/memory_stability.py --project-root P --workload W --server-bin B --workers-dir D --repeats N` replays a
workload against one server and reports whether residue grows with the number of calls.

## Runtime boundaries

- Keep each invariant in [architecture](docs/architecture.md) and its test true in the same commit. A new invariant
  names its test.
- Put code in the lowest package that owns its inputs, and ask Lake when one Lake command answers the question.
- Before adding a check, record, lock or scheduler, write one sentence naming the requirement it enforces and the native
  tool that already owns that work: Lake for builds, the kernel for locks and process exit, SQLite for atomic writes,
  Cargo for dependency overrides. If the native tool can enforce the requirement with a flag or one call, use that.
  Otherwise add one check at the one point where the requirement can fail, and delete it when the requirement goes.
- Never unlock `server.lock` or `children.lock`, and never signal a process group whose child file is free.
- Start every Lake run through the server's turns: builds through `lean_build`, reads through `lake_reader`. Never walk
  Lake's import graph.
- Only `crates/lean-host-mcp-worker` links `libleanshared`. The parent takes the worker crate's library with default
  features off, and Lean runtime types stay in the child. The parent's `build.rs` embeds Git identity only.
- Keep worker handles inside the `LeanProject` controller and borrowed `LeanWorkerSession<'_>` values in their stack
  frame. Never hold a session across `.await` or share a handle through `Arc<Mutex<_>>`.
- Route worker calls through the controller's deadline, cancellation, lifecycle and retry handling. Never retry a
  request whose execution may have started because the caller lost its result.
- Tie slot accounting to a worker's confirmed exit, failed starts and shutdown included.
- Keep axum and HTTP types in `transport_http.rs`. Stdout carries JSON-RPC while serving stdio, so log with `tracing`.
- RSS is telemetry only: add no RSS or request-count restart limit. Import count is not a sizing unit either; the import
  backstop only covers statistics that read zero. Keep the heap ceiling derived from the residue budget, not a fixed
  cap. Keep the pool size and the residue budget apart, and tie reuse hints and `imports_seen` to the pool size. Keep
  the guard against cycling with no work between, the supervisor's restart limit above planned cycling, and the actor's
  crash-loop breaker.

## Contracts and upkeep

- Lean parse, elaboration, kernel and meta failures are domain data; infrastructure failures are `ServerError` or
  runtime-unavailable results. Keep project identity, artifact freshness, execution outcome and proof verdict apart. A
  worker without proof-position support answers `unsupported`, not an error.
- Read the [tool catalog](docs/tool-catalog.md) before you change `tools/semantic.rs` or `envelope.rs`. The internal
  `Response<T>` is not the public schema, and request rows, `lean_build` and `lean_imports` keep their own shapes.
- `config_schema.rs` owns the generated table in `docs/operations.md` and the `config init` output: change the source
  and regenerate the table together.
- Clippy is strict. Use a concrete `reason = "..."` for an allowance production code cannot avoid. The PostToolUse hooks
  format files and print reminders; they are not gates. The architecture reviewer is read-only and checks these rules.
- Keep the README version matrix current. The root manifest declares the coupled lean-rs and lean-semantic-search
  versions once; wider Lean support starts in lean-rs.

## Release rule

Version by semver. While the version is 0.x, a breaking change to the tools, the configuration, the CLI or the library
bumps the minor version; any other change bumps the patch version. Record each change in `CHANGELOG.md`: history lives
there and in Git, not in the docs.

Test a change that spans lean-rs or lean-semantic-search and this repository together before you push or tag. Use
temporary `[patch.crates-io]` overrides; both installed-worker scripts take `LEAN_RS_CHECKOUT` and
`LEAN_SEMANTIC_SEARCH_CHECKOUT` and build the worker and server from those checkouts. Check the resolved crates with
`cargo metadata --format-version 1`, then run the affected Lean operation and `scripts/prerelease.sh`. Do not publish a
crate just to test it here, and do not commit local paths or an override lockfile. Once the whole change passes, release
upstream first and pin the published versions. The release and toolchain-bump skills in `.claude/skills/`, with aliases
in `.agents/skills/`, give the steps.

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
