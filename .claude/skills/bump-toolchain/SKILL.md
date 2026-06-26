---
name: bump-toolchain
description: Bump lean-host-mcp's supported Lean toolchain window (and the upstream lean-rs / lean-semantic-search releases it rides on). Use whenever the user wants to move lean-host-mcp to a newer Lean release, widen or advance the supported toolchain window, adopt a new lean-rs / lean-semantic-search version, change the toolchain the server is built and tested against, or extend toolchain support — even if they only say "bump the toolchain" without naming the coupled dependency work.
---

# Bump lean-host-mcp's supported Lean toolchain window

Unlike lean-dup (which pins a *single* toolchain), lean-host-mcp supports a **window** of Lean releases. A project
brings its own toolchain: the server hosts whatever version that project's `lean-toolchain` pins, as long as it falls
inside the supported window. Crucially, **the window is not declared in this repo** — it is read at runtime from the
`lean-toolchain` crate's `SUPPORTED_TOOLCHAINS`, which is itself sourced from
[`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). There is no `supported.rs`,
no header-digest list, and no CI version matrix to hand-edit here. `ToolchainId::window_verdict` (`src/toolchain.rs`)
just queries that crate.

So "bumping the toolchain" in lean-host-mcp is **almost entirely a dependency bump**. The window moves because the
`lean-toolchain` / `lean-rs-*` crates you depend on move. As the README puts it: *widening the window is a `lean-rs`
change first, then a version bump here.* You do not widen the window by editing this repo's code; you adopt a newer
upstream that already widened it, then move the one real pin (the test fixture) and the prose to match.

The whole job is: pick the upstream release line, move the single workspace dependency knob, move the fixture pin to the
new head, rebuild the worker, install it, test, and record it in docs + CHANGELOG.

## Before you start: identify the target

You need three coupled facts, and they come *from upstream*, not from a free choice:

1. The target **Lean toolchain** — the new head you want to build and test against (e.g. `leanprover/lean4:v4.32.0`). It
   must already be inside the window the upstream crates ship.
2. The `lean-rs` **release line** that includes that toolchain in its `SUPPORTED_TOOLCHAINS` (e.g. `0.3.x`) — this is
   the crates.io minor for `lean-rs-worker-{child,parent,protocol}` **and** the `lean-toolchain` crate (which exports
   the window).
3. The `lean-semantic-search` **release line** built on that same lean-rs line (e.g. `0.4.x`) — the crates.io minor for
   `lean-semantic-search-{capability,contract,retrieval,runtime}`.

**lean-rs and lean-semantic-search must advance on the same lean-rs line.** The comment in the root `Cargo.toml`
`[workspace.dependencies]` block spells out why: the lean-rs stack is 0.x (0.2.x / 0.3.x are mutually incompatible) and
lean-semantic-search re-exports lean-rs types in its public API, so a mismatched pair won't compile or will trip
`deny.toml`'s no-duplicate-version invariant. If the user gives you only a Lean version, find the matching upstream
lines by checking which lean-rs `lean-toolchain` includes it; if they give you only upstream tags, confirm the new head
they widen the window to. Don't proceed until all three line up — a mismatch is the most common failure (see *When it
fails*).

A point bump that merely refreshes the upstream patch *without* moving the head you test against is fine too — in that
case skip the fixture-pin move in step 3 and just bump the deps. When in doubt, assume the deps move.

## The ritual

### 1. Install the toolchain

```sh
elan toolchain install leanprover/lean4:vX.Y.Z
```

~500 MB; skip if already installed. You need it installed locally to build a worker and run the tests against it.

### 2. Move the upstream dependency pins — one knob

lean-host-mcp deliberately declares the version-coupled crate families **once**, in the root `Cargo.toml`
`[workspace.package]`/`[workspace.dependencies]` (members reference them via `.workspace = true`, so there is no
per-crate drift to chase). Bump the minors there:

- `lean-rs-worker-child`, `lean-rs-worker-parent`, `lean-rs-worker-protocol` — the worker stack.
- `lean-toolchain` — the crate that **exports the supported window**. This is the one whose bump actually moves the
  window; keep it on the same lean-rs line as the worker crates.
- `lean-semantic-search-capability`, `lean-semantic-search-contract`, `lean-semantic-search-retrieval`,
  `lean-semantic-search-runtime` — the semantic-search stack, on its matching line.

Then refresh the lockfile so the resolver picks them up:

```sh
cargo update -p lean-toolchain -p lean-rs-worker-parent -p lean-semantic-search-runtime   # …or `cargo update` for all
```

`deny.toml` forbids duplicate versions across the graph; if `cargo deny check` later complains about two lean-rs
versions, that means one family didn't advance with the other — go back and reconcile.

### 3. Move the fixture pin to the new head (skip for a pure patch bump)

There is exactly **one** authoritative `lean-toolchain` pin in this repo:

- `fixtures/lean/lean-toolchain` — CI reads this to `elan toolchain install` and to set `LEAN_SYSROOT`
  (`.github/workflows/ci.yml`). It is the toolchain the server is *built and tested against* — the head of the window.

Confirm it's the only real pin (fixtures may have been added):
`find . -name lean-toolchain -not -path '*/.lake/*' -not -path '*/target/*'`.

Separately, a handful of **test literals** in `src/` hardcode the head toolchain string to build synthetic fake projects
or serialization goldens — currently `server.rs`, `broker.rs`, and `tools/declaration_inventory.rs`. These are not pins
(no toolchain is installed for them), but keep them consistent with the fixture so the suite reads coherently:
`grep -rIn 'leanprover/lean4:v<old-head>' crates/*/src` finds them.

### 4. Update the Rust floor only if upstream raised it

The lean-rs worker crates pin a `rust-version`; adopting a new minor can raise lean-host-mcp's floor (it is currently
`1.91` in `[workspace.package]`). If upstream raised it, bump `rust-version` there and update any prose that names the
floor. If upstream didn't move it, leave it alone.

### 5. Rebuild, install the worker, and test

Mirror CI's order (`.github/workflows/ci.yml`). **Always build per-member, never `--workspace`** — a workspace build
unifies the `lean-rs-sys` feature set and silently re-links `libleanshared` into the parent, breaking the whole
multi-toolchain story (CLAUDE.md "Always build per-member").

```sh
# Lint gate first — CI runs clippy as -D warnings (clippy --workspace is safe; it doesn't link).
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Build parent and worker separately.
cargo build -p lean-host-mcp --all-targets
cargo build -p lean-host-mcp-worker --all-targets

# The parent must NOT link libleanshared. Assert it on a release build (per-member, never --workspace):
cargo build --release -p lean-host-mcp
! otool -L target/release/lean-host-mcp | grep -q libleanshared    # macOS
! ldd  target/release/lean-host-mcp | grep -q libleanshared        # Linux

# Parent unit/integration tests need no Lean fixture:
cargo test -p lean-host-mcp --all-targets
```

To exercise the new toolchain end to end, build a worker for it and point the parent at the built worker. The parent
resolves workers from `~/.local/share/lean-host-mcp/workers/<id>/`; during development shortcut that with
`LEAN_HOST_MCP_WORKERS_DIR` (CLAUDE.md "Running from a development checkout"):

```sh
# Build the worker for the new toolchain (its build.rs bakes the matching lib/lean rpath):
LEAN_HOST_MCP_TARGET_TOOLCHAIN=vX.Y.Z cargo build --release -p lean-host-mcp-worker

# Build the fixture's .olean artifacts so the e2e suite can load them:
( cd fixtures/lean && lake build )

# Opt-in end-to-end against the real fixture:
LEAN_HOST_MCP_WORKERS_DIR=$PWD/target/release \
LEAN_HOST_MCP_TEST_FIXTURE=$PWD/fixtures/lean \
    cargo test -p lean-host-mcp --test e2e -- --ignored
```

Or install the worker the way users do — `lean-host-mcp install-worker --toolchain vX.Y.Z` (it refuses out-of-window
pins up front, so a successful install confirms the window now covers it). If goldens/cache-keys in the suite shift
because the worker substrate moved, refresh them only when the change is the intended one, never to paper over a real
diff.

`scripts/prerelease.sh` runs all of the above plus `cargo deny` and the format checks in one pass — run it before
tagging (`--quick` skips cargo-deny for iteration).

### 6. Update docs and the CHANGELOG

- **README "Versions" section** is the single source of truth for the version matrix: it names the package version, the
  `lean-rs-worker-*` / `lean-rs` line, the supported window, and the head the server is built and tested against. Bring
  all of those in line with the bump.
- Grep for the *old head* to catch other prose and example mentions: `grep -rIn 'v<old-head>' README.md docs/ CLAUDE.md`
  (the JSON `lean_toolchain` examples in `README.md`, `docs/operations.md`, and `docs/tool-catalog.md` should track the
  head; `docs/ilean-reference-index.md`'s *"Verified … schema (Lean vX.Y.Z)"* note records the toolchain a schema was
  validated against — only move it if you re-verified under the new head).
- Add a `### Changed` bullet under `## [Unreleased]` in `CHANGELOG.md` naming the new head toolchain and the upstream
  release lines adopted.

### 7. Commit

Branch first if you're on `main`. Commit message in the repo's style, e.g.
`Move to lean-semantic-search 0.4 (lean-rs 0.3); widen window to vX.Y.Z` (mirror recent log entries). Summarize in the
body: the new head toolchain, the two upstream lines, any Rust-floor change, and the test result.

## When it fails

The failure modes here are about *pin and version alignment*, not ABI drift — there is no allowlist or version-specific
shim to reach for in this repo.

| Symptom | Cause | Action |
| --- | --- | --- |
| `BadProject`: *"lean-toolchain pins …, outside the lean-rs supported window …"* when opening a project or on `install-worker` | The depended-on `lean-toolchain` crate's window doesn't include that pin yet | Bump the `lean-toolchain` / `lean-rs-*` deps to a line whose `SUPPORTED_TOOLCHAINS` covers it (step 2). Do **not** try to widen the window by editing this repo — it's read from the crate. |
| `cargo deny check` reports duplicate `lean-rs` (or `lean-toolchain`) versions | The worker stack and lean-semantic-search ended up on different lean-rs lines | Reconcile both families onto the same line (step 2); `cargo update` to refresh `Cargo.lock`. |
| Parent binary links `libleanshared` (link-set assertion fails) | Built with `cargo build --workspace`, which unified the `lean-rs-sys` features into the parent | Rebuild per-member: `cargo build -p lean-host-mcp` / `-p lean-host-mcp-worker` (CLAUDE.md "Always build per-member"). |
| `libleanshared.{dylib,so}: cannot open` at worker startup | The worker was built for a different toolchain than is installed, so its rpath points at an absent prefix | Rebuild the worker with `LEAN_HOST_MCP_TARGET_TOOLCHAIN=vX.Y.Z` for the installed toolchain, or `install-worker --toolchain vX.Y.Z`. |
| A test golden / cache-key assertion fails only after the bump | The worker substrate version moved, or a hardcoded head literal is stale | If the diff is the intended additive change, refresh the golden; align the `server.rs` / `broker.rs` / `declaration_inventory.rs` head literals with the fixture pin (step 3). |
| A Rust test fails only on the new toolchain | Likely an upstream behavior change | Reproduce minimally; if it's an upstream regression, raise it with the lean-rs / lean-semantic-search maintainers rather than pinning around it here. |
