---
name: release-lean-host-mcp
description: Release checklist for lean-host-mcp. Use when cutting a release, bumping the version, or tagging a new lean-host-mcp version. Walks the reversible prep steps, then the irreversible signed tag.
disable-model-invocation: true
---

# Releasing lean-host-mcp

lean-host-mcp ships as a server/CLI, not a crates.io workspace. There is **no** `release.yml` yet — a release is a local
gate + version bump + CHANGELOG + a signed git tag. The tag is the release marker.

Work top to bottom. Steps 1–4 are reversible; step 5 (the tag push) is not — do it only after a human confirms.

## 1. Pre-flight gate

Run `scripts/prerelease.sh` from a clean tree (no uncommitted changes). It must pass end to end. Use `--quick` only for
iteration, never for the actual release gate — the full run is what asserts the parent ⊥ libleanshared link invariant
and dependency hygiene (cargo-deny).

## 2. Version bump (one source of truth)

Bump `[workspace.package].version` in the root `Cargo.toml`. Both crates inherit it via `version.workspace = true`, so
there is a single place to edit. If any `[workspace.dependencies]` entry pins an internal crate by version, update it in
lockstep. Run `cargo build` so `Cargo.lock` updates.

## 3. CHANGELOG

There is no `CHANGELOG.md` yet. On the first release, create one with a `## [X.Y.Z] - YYYY-MM-DD` heading; thereafter
move the `## [Unreleased]` section into the new dated heading. The tag message must match the version.

## 4. Version matrix check

Confirm the supported `lean-rs` / Lean toolchain pairing in the **README** (the single source of truth) is current for
this release. Bumping the toolchain is a `lean-rs` change first, then a version bump here — see CLAUDE.md "Version
matrix".

## 5. Tag (irreversible)

After human confirmation:

```sh
git tag -s vX.Y.Z -m "lean-host-mcp vX.Y.Z" && git push origin vX.Y.Z
```
