# Lean 4.36 migration audit

Recorded 2026-11-17 against clean source revision `7b868df8ded9d813074afd599bb8d1e0b914bca2` in an isolated
worktree. This is a preparation record; Lean 4.36 runtime compatibility and release qualification remain pending.

The official [Lean 4.36.0 release](https://github.com/leanprover/lean4/releases/tag/v4.36.0), published 2026-11-16,
points to `611c117bfaf7b7ae1cfcf246ce6fba31e8f509e9`. Its availability does not extend this host's supported window.

## Current dependency and pin matrix

| Input | Audited value | Migration requirement |
| --- | --- | --- |
| Host package | `0.16.8` | Decide its release version after the upstream handoff. |
| `lean-rs-worker-{parent,child,protocol}`, `lean-toolchain` | Root Cargo requirements and lock entries `0.9.10` | Adopt one coordinated SDK family with an audited 4.36 entry in `lean_toolchain::SUPPORTED_TOOLCHAINS`. |
| `lean-semantic-search-{capability,contract,retrieval,runtime}` | Root requirements and lock entries `0.7.8` | Adopt a qualified semantic family using the same SDK line. |
| Worker wire protocol | `13` | Parent and child must agree; a Lean version change alone does not choose a protocol bump. |
| Supported Lean head | `4.35.0`, floor `4.30.0` | Widen upstream first; keep numbered unsupported pins rejected. |
| Tracked Lean pin | `fixtures/lean/lean-toolchain`: `leanprover/lean4:v4.35.0` | Move with the coupled dependencies after handoff. There is no root Lean pin. |
| Fixture Lake manifest | Tracked `version: 1.1.0`, no packages | Keep the fixture independent of host shim/semantic package requirements. |
| Rust floor | `1.94` | Change only if the selected upstream families require it. |

The SDK source snapshot inspected was `a24394e6b5c2330ccf3d81a67cea0bc94b103abc`; semantic source was
`1ac78f72f2647d74b722eeada53adc8bc5df44a7`. These source identities do not establish the provenance of a
registry-named lock entry. The root manifest has no patch table, and `.cargo/config.toml` contains profile settings.
The SDK and semantic lock entries currently have registry source names but no registry checksums.

The warm-worker owner supplied a separate historical test handoff: SDK
`4ae59cd0e69ceccd8aae1f24532df107fdbcd409` (`0.9.10`) and semantic
`1ac78f72f2647d74b722eeada53adc8bc5df44a7` (`0.7.8`) were selected through an explicit, disposable Cargo
`--config` override covering all eight SDK crates and the four semantic crates above. Those tests used fake workers,
4.35 header metadata and a zero fixture digest. They establish neither native bootstrap nor 4.36 support. Their
registry-only offline attempt offered cached `0.9.9`, not the requested `0.9.10`; that is not an online publication
check. Preserve this distinction when reporting a private build. A proposed SDK `0.10.0` is still a joint API/ABI release
decision, not an adopted migration pin.

The SDK 4.36 lane's probe-fix commit is `ada669039335eb3616413c07c4adc32ffbcb41ff`. Its Apple Silicon macOS
release-library audit and direct C link probes found only 86 of the 88 required symbols: `lean_string_data` and
`lean_string_push` are missing on 4.36 and link on 4.35. Measured layouts match, but the export failures block the
current contract. The SDK's 4.36 support-metadata patch remains unapplied. Resolve the public ABI with its owner before
adopting a pin; do not waive these missing exports or treat the probe-fix commit as 4.36 support.

The semantic preparation branch reached `333c2198f30560c3f3ce662c04c1fbda3325795b`, with packaged source revision
`31cc612937767191748b395ad447d35897244a29` and digest
`ee01ce0d968b9ec34726cae9dfd135c4b1378af31e5a20c41a14ce84ae7b1176`. Its owner reports a passing one-file API,
JSON and generated-signature probe; full package tests, Rust qualification and a release version/tag remain pending.
These are prepared-source facts, not the final dependency pair. That lane's online registry-resolution attempt also
failed to find the required SDK protocol `0.9.10`; private local qualification and registry reproducibility remain
separate gates.

## Export, artifact and installation boundaries

| Surface | Owner and requirement | Evidence still needed on 4.36 |
| --- | --- | --- |
| Lean C ABI, layouts, header digest and required symbols | SDK 4.36 lane, coordinated with the active ABI repair | Audited table entry, layout/inline review, required-export check and real runtime tests. |
| Typed host shim bindings | SDK's closed signature declarations and worker child | Matching manifests and loaded bindings before each body command. The compiled required subset currently names `process_module_query_batch_cached`, `verify_declaration_batch`, and `runtime_version`. |
| Semantic capability | Semantic runtime's packaged Lean source and five typed export signatures | Built exports for metadata, doctor, declaration features, proof-goal features and streamed declaration features. Refresh and verify packaged provenance with source changes. |
| Parent executable | Host transport, broker and serialized controller | A separately built release parent must have no `libleanshared` dependency. |
| Per-toolchain worker | Only the worker links Lean | Matching header, worker rpath, protocol and runtime smoke result; old `.olean` files cannot qualify a new toolchain. |
| Consumer Lake project | Project's own pins, manifest and built imports | No consumer dependency on host shims or the semantic runtime package. |

Static inspection matched all five semantic export names in the capability declarations to the packaged Lean
`@[export]` declarations. This does not check a compiled library's exports or argument ABI. Optional proof-position
support must continue to degrade according to the tool contract; an absent required binding remains an error.

The official 4.35 and isolated official 4.36 source files `Lean/Server/References.lean`,
`Lean/Data/Lsp/Internal.lean`, and `lake/Lake/Load/Manifest.lean` were byte-identical. This supports retaining the
existing `.ilean` v5 reader and manifest parser for qualification. No 4.36 fixture was built or read, and the historical
schema-verification entry in [architecture.md](architecture.md#supported-v5-schema) was not relabeled.

Read-only local installation inspection found no canonical 4.36 elan toolchain or installed 4.36 host worker. The
installed 4.35 worker's sidecar records host `0.16.8`, protocol `13`, header
`eb951e1171d39b828e7ad60aed0b90759598b678bfe58cf85c65a1559c021bf9`, and a past passing smoke test. This is
recorded installation evidence, not a fresh loaded-runtime receipt. No installed executable, worker, launcher or
service was changed by this audit.

## Coordinated changes after upstream handoff

1. Obtain the reviewed SDK and semantic revisions, release versions, exported window and packaged-source provenance.
   The SDK 4.36 lane owns support metadata, the ABI lane owns ABI repair, and the warm-worker lane owns its private pin
   and lifecycle changes. This audit changes none of those surfaces.
2. Update all eight coupled root Cargo requirements and refresh only their relevant lock graph. Reconcile duplicate
   SDK lines without relaxing dependency or ABI gates. Keep any private overrides disposable and explicitly identified.
3. Move the fixture pin; update the [README matrix](../README.md#versions) and Unreleased changelog. Audit old-head
   literals rather than replacing historical examples indiscriminately. In particular,
   `scripts/test_writer_handoff.py` currently asserts and creates 4.35 projects; its lifecycle owner must align that
   harness before counting it as 4.36 evidence.
4. Qualify under a coordinated build lane, with at most two build jobs. Export the exact 4.36 `LEAN_SYSROOT` for the
   entire Rust build/test run and set `LEAN_HOST_MCP_TARGET_TOOLCHAIN=v4.36.0` for the worker. A target-toolchain rpath
   alone does not select the header fingerprint. Use isolated target, fixture and worker directories.

## Validation matrix

| Check | Result for this audit | Remaining qualification |
| --- | --- | --- |
| Git state, tracked pins, Cargo family/lock inventory | Inspected; canonical checkout preserved | Recheck the integrated revision. |
| `cargo metadata --format-version 1 --no-deps --frozen` | Passed, two workspace members | This parses workspace metadata only; it does not resolve or compile the full dependency graph. |
| `cargo fmt --all -- --check` | Passed | Repeat after source integration. |
| Git archive | Baseline manifest, lock and fixture pin match the worktree | Test the final committed/exported graph against its intended dependency source. |
| Markdown lint and `git diff --check` | Passed for the audit changes | Keep the audit synchronized with the adopted source pair. |
| Semantic source export names and Lean reader/manifest sources | Static checks passed as described above | Compiled exports, fixture `.ilean` and loaded ABI checks remain pending. |
| Per-member build, strict Clippy, parent tests and HTTP transport tests | Deferred | Use the adopted SDK/semantic pair with the explicit 4.36 sysroot. Never link a workspace build. |
| Separate release parent/worker builds and parent link assertion | Deferred | Required before runtime qualification. |
| 4.36 fixture build and ignored e2e tests | Deferred | Point `LEAN_HOST_MCP_WORKERS_DIR` at the isolated release worker directory and `LEAN_HOST_MCP_TEST_FIXTURE` at the isolated fixture. |
| Readiness negative cases and runtime smoke | Deferred | Keep unsupported/header/protocol/failed-smoke refusals and optional capability behavior intact. |
| Full `scripts/prerelease.sh` | Deferred | Required before tagging; an iteration with `--quick` is not the full gate. |

No Rust compilation, Lean fixture build, worker installation, deployment, push, proof-repository change or Mathlib/cache
operation was performed. Integration is blocked by the SDK's missing required exports, the exact reviewed SDK/semantic
handoff, and its coordinated build lane.
