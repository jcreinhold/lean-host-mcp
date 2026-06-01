---
name: release-lean-host-mcp
description: Release checklist for lean-host-mcp. Use when cutting a release, bumping the version, or publishing a new lean-host-mcp version to crates.io. Walks the reversible prep steps, then the irreversible signed tag and crates.io publish.
disable-model-invocation: true
---

# Releasing lean-host-mcp

lean-host-mcp publishes **both** crates to crates.io (`lean-host-mcp` and `lean-host-mcp-worker`). Publishing is
**CI-driven**: `.github/workflows/release.yml` triggers on a `v<semver>` tag push, re-runs the full gate, then publishes
both crates (worker first, `--no-verify`; then the parent) and creates a GitHub Release from the matching CHANGELOG
section. A release is therefore a local gate + version bump + CHANGELOG + a signed git tag — the **tag push is the
trigger**, not a manual upload.

Work top to bottom. Steps 1–4 are reversible; step 5 (the tag push) is **not** — it kicks off the irreversible crates.io
publish, and crates.io versions are permanent (yank-only). Do it only after a human confirms.

**One-time setup:** the repo must have a `CARGO_REGISTRY_TOKEN` secret (a scoped publish token from
<https://crates.io/settings/tokens>); the publish job fails fast with a clear message if it is absent. Rehearse the
whole pipeline without uploading by running the workflow via `workflow_dispatch` with `dry_run: true`.

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

## 5. Tag and push (triggers the irreversible publish)

First push the release commit, then push the matching signed tag. The workflow runs the gate and publishes from the
**tagged commit**, so `release.yml` (and the CHANGELOG section for this version) must already be in it.

After human confirmation:

```sh
git push origin main
git tag -s vX.Y.Z -m "lean-host-mcp vX.Y.Z" && git push origin vX.Y.Z
```

The tag's name must equal `v` + the workspace version, or the `verify` job fails the tag-vs-version check before
anything uploads.

## 6. Watch the release workflow

The tag push starts `.github/workflows/release.yml`. It re-runs the gate (`verify`), then `publish` uploads **worker
first** (`--no-verify`, since its verify build would link `libleanshared`), then the **parent** (verifies normally — it
never links `libleanshared`, so the publish job needs no Lean toolchain), and finally creates the GitHub Release from
the `## [X.Y.Z]` CHANGELOG section. Worker-before-parent is a user-experience rule (a user must never install a parent
whose `install-worker` resolves a not-yet-published worker), not a cargo constraint — the two crates have no inter-crate
dependency.

Watch the run (`gh run watch` or the Actions tab). crates.io versions are permanent — a mistake can only be yanked, not
replaced.

**Recovery.** If the run dies between the two uploads (e.g. the parent loses the index-propagation race), re-run the
workflow via `workflow_dispatch` (`dry_run: false`). The publish step is idempotent: it skips any crate whose version is
already on crates.io and uploads only the missing one, so it completes the release without burning a version.
