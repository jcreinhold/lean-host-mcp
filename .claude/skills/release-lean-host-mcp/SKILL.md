---
name: release-lean-host-mcp
description: Release checklist for lean-host-mcp. Use when cutting a release, bumping the version, or publishing a new lean-host-mcp version to crates.io. Walks the reversible prep steps, then the irreversible signed tag and crates.io publish.
disable-model-invocation: true
---

# Releasing lean-host-mcp

lean-host-mcp publishes **both** crates to crates.io (`lean-host-mcp` and `lean-host-mcp-worker`). There is **no**
`release.yml` yet — a release is a local gate + version bump + CHANGELOG + a signed git tag + a manual `cargo publish`.

Work top to bottom. Steps 1–4 are reversible; step 5 (the tag push) and step 6 (the crates.io publish) are **not** —
crates.io versions are permanent (yank-only). Do them only after a human confirms.

## 1. Pre-flight gate

Run `scripts/prerelease.sh` from a clean tree (no uncommitted changes). It must pass end to end. Use `--quick` only for
iteration, never for the actual release gate — the full run is what asserts the parent ⊥ libleanshared link invariant
and dependency hygiene (cargo-deny).

Then validate packaging without uploading:

```sh
cargo package --list -p lean-host-mcp           # must include README.md (crate-local)
cargo package --list -p lean-host-mcp-worker    # must include Cargo.lock (else `install --locked` fails for users)
cargo publish --dry-run -p lean-host-mcp
cargo publish --dry-run --no-verify -p lean-host-mcp-worker
```

`--no-verify` on the worker skips the verify-build that links `libleanshared` (it needs a Lean toolchain in the env).

## 2. Version bump (one source of truth)

Bump `[workspace.package].version` in the root `Cargo.toml`. Both crates inherit it via `version.workspace = true`, so
there is a single place to edit. If any `[workspace.dependencies]` entry pins an internal crate by version, update it in
lockstep. Run `cargo build` so `Cargo.lock` updates.

## 3. CHANGELOG

Move the `## [Unreleased]` section in `CHANGELOG.md` into a new `## [X.Y.Z] - YYYY-MM-DD` heading and update the compare
links at the bottom. The tag message must match the version.

## 4. Version matrix check

Confirm the supported `lean-rs` / Lean toolchain pairing in the **README** (the single source of truth) is current for
this release. Bumping the toolchain is a `lean-rs` change first, then a version bump here — see CLAUDE.md "Version
matrix".

## 5. Tag (irreversible)

After human confirmation:

```sh
git tag -s vX.Y.Z -m "lean-host-mcp vX.Y.Z" && git push origin vX.Y.Z
```

## 6. Publish to crates.io (irreversible)

After the tag is pushed, publish the exact tagged commit. **Worker first**, then the parent — a user must never be able
to install a parent whose `install-worker` resolves a not-yet-published worker:

```sh
cargo publish --no-verify -p lean-host-mcp-worker   # --no-verify: its verify-build would link libleanshared
cargo publish -p lean-host-mcp                       # parent verifies normally (never links libleanshared)
```

The two crates have no inter-crate dependency, so this ordering is a user-experience rule, not a cargo constraint.
crates.io versions are permanent — a mistake can only be yanked, not replaced.
