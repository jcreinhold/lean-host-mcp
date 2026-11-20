---
name: release-lean-host-mcp
description: Prepare and validate a lean-host-mcp release, then publish both crates through the signed-tag CI workflow only after explicit human confirmation of the release push.
disable-model-invocation: true
---

# Release lean-host-mcp

[release.yml](../../../.github/workflows/release.yml) verifies a `v<semver>` tag, publishes the worker before the
parent, and creates a GitHub Release from the matching changelog section. Both crates inherit the root workspace
version. A tag push triggers permanent crates.io publication; obtain explicit human confirmation before pushing the
release commit and signed tag. Prepare the exact commit, tag name, and check results first.

## Prepare

Inspect the working tree and preserve unrelated changes. A release must use published dependencies: check the root
manifest for any `[patch.*]` table, including local lean-rs overrides. If present, publish the required upstream fixes,
remove the overrides, resolve the published versions, and rebuild/reinstall affected workers before proceeding. The
release workflow also rejects patches; the development prerelease script allows them.

Update `[workspace.package].version` in root `Cargo.toml` and any versioned internal dependency entries. Refresh the
lockfile with a per-member build (`cargo build -p lean-host-mcp`). Keep parent and worker builds separate so feature
unification cannot link `libleanshared` into the parent.

Move Unreleased entries into `## [X.Y.Z] - YYYY-MM-DD`, retain an empty Unreleased section, and update the changelog
compare links. Check the README version matrix against the resolved dependencies and fixture toolchain. Use the
toolchain-bump skill if that pairing changes.

Inspect the scoped diff and commit the release preparation so the validation starts from a clean tree. Do not include
another worker's staged files.

## Validate the release commit

Run `scripts/prerelease.sh` in full. `--quick` skips cargo-deny and is only for iteration. Use the fixture's Lean
toolchain and export its `LEAN_SYSROOT` for the full invocation as described in `AGENTS.md` and CI.

Check packaging explicitly:

```sh
cargo package --workspace --locked
```

Cargo verifies each packaged member with access to the archives of selected workspace dependencies.
Check the parent archive for its crate-local README and the worker archive for Cargo.lock, as Verify does.
External SDK and semantic dependencies must resolve from the configured registry.

Confirm `CARGO_REGISTRY_TOKEN` is configured for the workflow without printing its value. A manual workflow run with
`dry_run: true` rehearses verification and packaging without publishing. Present the exact release commit, matching
`vX.Y.Z` tag, changelog, and validation results for approval.

## Publish after confirmation

```sh
git push origin main
git tag -s vX.Y.Z -m "lean-host-mcp vX.Y.Z"
git push origin vX.Y.Z
```

The tag must match the workspace version and identify the verified release commit. The workflow's Lean-equipped `verify`
job checks both packages, then the Lean-free `publish` job uploads worker first and parent second with `--no-verify`,
relying on that verification. Watch the run through completion and report the published versions and release URL.

If publication stops after one crate, inspect the failure before retrying. A manual run with `dry_run: false` skips
versions already present on crates.io and uploads only the missing crate. Published versions cannot be replaced; a bad
release can only be yanked.
