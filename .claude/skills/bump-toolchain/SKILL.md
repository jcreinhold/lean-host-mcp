---
name: bump-toolchain
description: Update lean-host-mcp's supported Lean toolchain window, fixture head, or coupled lean-rs and lean-semantic-search dependency lines. Use for toolchain or upstream-stack bumps; the supported window must already exist upstream.
---

# Update toolchain support

The supported window comes from `lean_toolchain::SUPPORTED_TOOLCHAINS`, supplied by lean-rs. This repository adopts that
window through dependency versions; it does not maintain a separate allowlist. The README version matrix is the
reader-facing reference, and `fixtures/lean/lean-toolchain` is the CI build/test pin.

## Align the upstream releases

Establish the target Lean head, the lean-rs line that supports it, and the compatible lean-semantic-search line. The
latter exposes lean-rs types, so the two families must agree. Check upstream manifests and the exported window rather
than guessing from version numbers. For a patch refresh that leaves the tested head unchanged, retain the fixture pin.

Update the coupled entries in root `Cargo.toml` under `[workspace.dependencies]`: `lean-toolchain`,
`lean-rs-worker-{parent,child,protocol}`, and `lean-semantic-search-{capability,contract,retrieval,runtime}`. Members
inherit them. Refresh `Cargo.lock` with targeted `cargo update -p ...` calls and reconcile duplicate-version failures
instead of weakening `deny.toml`.

Move `fixtures/lean/lean-toolchain` when the tested head changes. Use `rg --files --hidden -g lean-toolchain` to check
for additional tracked pins, and search `crates/` for old-head test literals; synthetic project strings are not pins.
Raise `[workspace.package].rust-version` only if required by the adopted dependencies, and align any stated Rust floor.

## Build and verify

Install the target toolchain if needed. Export its sysroot before **all** builds and tests: dependency build scripts
also bake its `lean.h` fingerprint into parent test binaries. `LEAN_HOST_MCP_TARGET_TOOLCHAIN` alone sets only the
worker rpath and can produce `lean_rs.loader.unsupported_toolchain_fingerprint` at bootstrap.

```sh
elan toolchain install leanprover/lean4:vX.Y.Z
export LEAN_SYSROOT="$HOME/.elan/toolchains/leanprover--lean4---vX.Y.Z"
export LEAN_HOST_MCP_TARGET_TOOLCHAIN=vX.Y.Z

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p lean-host-mcp --all-targets
cargo build -p lean-host-mcp-worker --all-targets
cargo test -p lean-host-mcp --all-targets
cargo build --release -p lean-host-mcp
cargo build --release -p lean-host-mcp-worker
```

Never build with `--workspace`: feature unification can link Lean into the parent. Assert the parent link set with the
platform command in `AGENTS.md` after its separate release build.

Build the fixture, then exercise the matching developer worker:

```sh
(cd fixtures/lean && lake build)
LEAN_HOST_MCP_WORKERS_DIR=$PWD/target/release \
LEAN_HOST_MCP_TEST_FIXTURE=$PWD/fixtures/lean \
    cargo test -p lean-host-mcp --test e2e -- --ignored
```

Alternatively, install through `lean-host-mcp install --lake-root fixtures/lean` and test that installed worker; it
reads the toolchain from the fixture's `lean-toolchain`. `scripts/prerelease.sh` adds documentation, formatting,
link-set, and dependency checks; its full run is required before tagging. Refresh goldens only after confirming the
changed behavior is intended.

## Record the result

Update the README version matrix and add an Unreleased changelog entry naming the adopted lines and tested head. Search
current documentation for old versions, keeping historical changelog entries and dated schema-validation evidence unless
re-verified. Do not change an `.ilean` schema verification date/version without checking that schema.

Commit the scoped changes with the adopted versions and validation result. If the window rejects the pin, update
upstream first. If runtime loading fails, check the installed toolchain, worker rpath, and header fingerprint before
changing compatibility policy. Minimize an upstream regression instead of silently pinning around it.
