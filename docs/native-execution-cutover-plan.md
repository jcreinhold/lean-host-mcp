# Native execution cutover plan

Status: **source-reviewed plan; no complete service, cutover or deployment qualification.** Read-only inventory on
2026-11-17 used host `3a310c9`, SDK `4ae59cd`, Proofs `5d867f7` and lean-fmt `81899c2`. Sibling-repository links below
assume checkouts beside this repository. This plan supplements the
[lifecycle design](native-execution-lifecycle-design.md) and
[test-only stage-store contract](../specs/native-execution/stage-store-contract.md); it does not implement them.

The cutover must remove old ownership roles. A service above unchanged Python/helper controllers would add another
layer. Keep Proofs acceptance and provenance, semantic project/session reuse, and the SDK's qualified process/group,
guardian, stable-lock and owner-registry mechanisms. Put native admission/expiry/recovery behind one small client. Do
not add another custody database, manifest tree, permanent recovery daemon or Proofs execution queue.

## Verified ownership and deletion targets

There are **three Python native-helper custody paths**, not just writer and external reader:

| Current path and active caller | Source evidence | Required disposition after parity |
| --- | --- | --- |
| Writer | [OwnedWriter.__enter__](../../proofs/scripts/stacks/src/stacks/owned_writer.py#L401) starts `writer` with `Popen` and retains control/wait ownership; [managed_lake](../../proofs/scripts/stacks/src/stacks/orchestration/managed_lake.py#L42) calls `configured_writer`. | Replace process/control ownership with submit/get/cancel. Preserve fixed stages, source checks and historical writer receipts. Remove inactive `checked_writer` after verifying no callers. |
| External reader | [run_reader](../../proofs/scripts/stacks/src/stacks/native_reader.py#L248) owns cancel pipe, helper `Popen`, timeout and wait; [managed_read](../../proofs/scripts/stacks/src/stacks/orchestration/managed_read.py#L172) calls it despite the reader module's stale “inactive” description. | Move helper lifetime and expiry below the native boundary; retain exact command/input/domain descriptions and truthful unknown results. |
| Parent artifact-read proxy | [read_artifact_traces](../../proofs/scripts/stacks/src/stacks/artifact_reads.py#L191) starts `read-proxy` at line 252 and reads trace metadata in the parent; [compilation](../../proofs/scripts/stacks/src/stacks/orchestration/compilation.py#L487) consumes it. | Include proxy FIFO/control/wait ownership in deletion. Preserve trace checks; use a native read operation or service-owned scope rather than a detached proxy controller. |

All three reserve the shared [_HelperWaitOwner](../../proofs/scripts/stacks/src/stacks/owned_writer.py#L98), which
starts a daemon thread and waits on the actual helper. “Three” counts these normal adapter families, not every Python
subprocess or every native process. Protocol queries, test fixtures and semantic transport have different roles.

The matching native ownership paths also require consolidation:

- Host [CLI dispatch](../crates/lean-host-mcp/src/main.rs#L44) exposes `writer`, `writer-exec`, `reader`, `read-proxy`,
  `native-reader`, `native-reader-exec` and `native-guardian`. Hidden CLI names are not fences. The
  [writer controller](../crates/lean-host-mcp/src/shared_writer.rs#L1189) retains EX and blocks on control input after
  `locked`; its command deadline begins inside
  [execute_with_launcher](../crates/lean-host-mcp/src/shared_writer.rs#L995). Replace superseded controller loops,
  preserving the blocked launch gate and actual closure checks.
- SDK [SharedWriter](../../lean-rs/crates/lean-toolchain/src/shared_writer.rs#L725) launches the helper and retains
  `_controller_locks` EX aliases. [SharedReader](../../lean-rs/crates/lean-toolchain/src/shared_reader.rs#L526) and
  [SharedReadProxy](../../lean-rs/crates/lean-toolchain/src/shared_reader.rs#L843) launch separate helper backends. Thin
  compatibility adapters may keep useful command names; they must submit to the sole owner with no legacy fallback.
- Broker [worker_slots/capacity_admission](../crates/lean-host-mcp/src/broker.rs#L301) and
  [reserve_worker_slot](../crates/lean-host-mcp/src/broker.rs#L1221) currently make grant decisions. Move global policy
  into the service; keep broker profile selection and idle-retirement signals. Guardian-backed native slots remain
  physical-occupancy validation, not a second configurable grant policy.
- Preserve [warm generation configuration](../crates/lean-host-mcp/src/project.rs#L3500) and the SDK's actual-child
  [supervisor](../../lean-rs/crates/lean-rs-worker-parent/src/supervisor.rs#L1784),
  [process-group](../../lean-rs/crates/lean-toolchain/src/process_group.rs#L48) and
  [guardian](../../lean-rs/crates/lean-toolchain/src/native_guardian.rs#L175) mechanisms below the selected scope API.
  One generation spans sequential semantic requests; request completion does not release its custody.

## Launcher closure set

Before enabling new submissions, classify every supported path below as migrated, refused, or an explicit bounded
exception. This is a local supported-workflow inventory, not a claim to revoke every arbitrary Lean executable.

| Surface | Concrete paths | Fence or retained boundary |
| --- | --- | --- |
| Semantic MCP | Proofs [mcp-serve](../../proofs/Makefile#L98), `LEAN_HOST_MCP` override; host default/`serve`, stdio/HTTP and embedded [ProjectBroker](../crates/lean-host-mcp/src/lib.rs#L56) | Verify the endpoint owner and matched native capability. Updating PATH does not replace a running endpoint or an explicit executable. |
| Offline and ordinary orchestration | [offline_check](../../proofs/scripts/stacks/src/stacks/orchestration/offline_check.py#L21), [managed_lake](../../proofs/scripts/stacks/src/stacks/orchestration/managed_lake.py#L42), [managed_read](../../proofs/scripts/stacks/src/stacks/orchestration/managed_read.py#L172), build/lint/clean/regroup/profile/tooling/blueprint callers | Migrate common adapters and all callers. Replace offline helper/registry flags with service discovery independent of MCP; unavailable service defers work, never starts an old helper. |
| Preparation and hidden writes | Host [host_preparation](../crates/lean-host-mcp/src/host_preparation.rs#L145), [Lake option elaboration](../crates/lean-host-mcp/src/lake_options.rs#L220), [semantic runtime build](../crates/lean-host-mcp/src/project.rs#L3671); SDK [build_helpers](../../lean-rs/crates/lean-toolchain/src/build_helpers.rs#L854), [source_package](../../lean-rs/crates/lean-toolchain/src/source_package.rs#L247) | Include shim/runtime builds and metadata Lean execution. Preserve no-build readiness; a stopped preparer does not prove independent writer closure. Typed private Cargo bootstrap needs its own explicit boundary. |
| Formatter | Proofs [formatting adapter](../../proofs/scripts/stacks/src/stacks/orchestration/formatting.py#L447); lean-fmt [finite managed mode](../../lean-fmt/LeanFmt/Cli.lean#L466), [child spawning](../../lean-fmt/LeanFmt/Application.lean#L421), [install targets](../../lean-fmt/Makefile#L53) | Retain inherited-group, no-build and validation contract for finite service payloads. Unsupported images refuse; never retry without flags. Standalone, watch, LSP, compiler build and clean are separate paths, not implicitly fenced. |
| Architecture checks | Proofs [make check-architecture](../../proofs/Makefile#L136) runs `lake run` through [lakefile script](../../proofs/lakefile.lean#L67); [check_architecture.py](../../proofs/scripts/check_architecture.py#L138) directly runs `lake exe runLinter`/`import-health` for Lean targets. | Migrate/refuse elaborated execution. Even docs-only Make first evaluates Lake; direct `uv run scripts/check_architecture.py docs/...` is a separate text-only route. Do not use the Make route while Lean work is paused. |
| Direct Lean/Lake and operator tools | Proofs [proof_audit](../../proofs/scripts/lean/proof_audit.py#L114), [reapers/lean-kill](../../proofs/Makefile#L90), [CI cache script](../../proofs/scripts/ops/build-cache.sh#L6), [header-only deps parser](../../proofs/scripts/lean/dag_traversal.py#L60) | Route supported elaborated metrics and direct [Lake executable targets](../../proofs/lakefile.lean#L113) through native ownership or refuse. Replace routine heuristic reaping with scope operations; no fallback invokes it. Isolated CI and verified header-only parsing can remain named exceptions, not generic shell execution escapes. |
| Hooks/editor | Proofs [.claude formatter hook](../../proofs/.claude/hooks/format.sh#L11), [.codex formatter hook](../../proofs/.codex/hooks/format.sh#L11), [.claude settings](../../proofs/.claude/settings.json#L17); enabled editor/LSP commands | Inspected hooks do not run Lean. Direct `Bash(lake *)` is allowed by current agent settings; permission policy is not a custody fence. Disabled global `lean-lsp-mcp` must remain outside the supported guarantee unless deliberately migrated. Editor defaults were not exhaustively inspected. |
| Executable discovery/dev/tests | Host [worker resolver](../crates/lean-host-mcp/src/toolchain.rs#L199), [installer](../crates/lean-host-mcp/src/cli/install_worker.rs#L591), [memory harness](../scripts/memory_stability.py#L84), [handoff harness](../scripts/test_writer_handoff.py#L394); SDK [child discovery](../../lean-rs/crates/lean-rs-worker-parent/src/capability.rs#L2199) | Include explicit/env/sibling/developer worker paths and app-owned binaries. Keep deliberate old-protocol negative fixtures isolated. Generic advanced SDK direct-runtime APIs remain outside managed guarantees unless their callers migrate. |

## Existing checks and the new fence

These identities have different jobs:

| Identity | Current evidence or proposed requirement |
| --- | --- |
| Package version, revision, file hash | Build provenance and exact image identity; none alone proves the running endpoint or grants execution. |
| Worker wire 13, Lean/header/binding fingerprints | Current [worker protocol](../../lean-rs/crates/lean-rs-worker-protocol/src/protocol.rs#L39) and [loaded compatibility](../crates/lean-host-mcp/src/runtime_compatibility.rs#L202); preserve actual per-generation validation. |
| Artifact-read epoch; semantic request epoch | Current custody/reuse and retained-ticket bindings respectively; neither is an installation fence. |
| Store UUID, acceptance window, nonce, mutation capability | Proposed native attempt identity and mutation authority, persisted with original job/stage/explicit revision before CREATE. Closed windows cannot create missing IDs; existing IDs remain evidence. |
| Installation/protocol epoch and service incarnation | Proposed sole-service fencing at new launch gates before GO; final packaging/discovery contract still needs implementation and qualification. |

This distinction is source-backed: host [WriterContract](../crates/lean-host-mcp/src/shared_writer.rs#L46) contains only
protocol, executable and registry; Proofs
[native_reader_configuration](../../proofs/scripts/stacks/src/stacks/native_compatibility.py#L200) explicitly says it
neither fences a later restart nor reserves a slot. The
[reader argv](../../proofs/scripts/stacks/src/stacks/native_reader_protocol.py#L122) carries no service incarnation or
installation epoch; [configured writer launch](../../proofs/scripts/stacks/src/stacks/owned_writer.py#L426) directly
starts protocol-1 `writer`. Worker protocol 13 has no such installation gate either.

New capability checks therefore cannot retroactively fence unchanged binaries. Stop old submissions, finish or
explicitly cancel known old scopes, observe closure, and update every supported launcher before enabling the new path. A
manually invoked old cooperating writer remains outside that supported set; common locks protect only where honored.
Never unlink/rotate canonical artifact locks or remove the SDK owner-record gate to manufacture progress.

Smallest packaging target: one matched host/service image, its app-owned per-toolchain workers and one client path. Use
ordinary release metadata for hashes/protocols; reuse the native owner registry for custody. The proposed
`execution-service` command, singleton, short discovered socket and installation epoch are not present in the inspected
CLI. Keep packaging decisions inside the native boundary rather than exposing guardian/FD/heartbeat phases to clients.

## Installed snapshot, not runtime qualification

The following mutable facts were observed read-only on **2026-11-17 UTC**; no installed executable or socket was
invoked.

| Observed file/configuration | Evidence and limit |
| --- | --- |
| `/Users/jcreinhold/.cargo/bin/lean-host-mcp` | SHA-256 `5281cb27eaa6e18434e9f55e9fb0ed317b0eb1a340b1bc4ad9a7d91fc19bcc24`; Cargo metadata claims 0.16.7, while inspected host source declares 0.16.8. Neither identifies the image serving HTTP. |
| `/Users/jcreinhold/Library/Application Support/lean-host-mcp/workers/v4.35.0/lean-host-mcp-worker` | SHA-256 `08ed37476c8b196105988eb3a942b1bf6d10c944bc9eca7a9858d125905899a4`; adjacent sidecar claims host 0.16.8, protocol13 and passed historical smoke. No smoke rerun. Source uses platform `data_local_dir`, not a universal Linux-style install path. |
| Formatter images | PATH `/Users/jcreinhold/.local/bin/lean-fmt` has SHA-256 `e02fe94046fa6a5ecbc5f84b7f90133019600951b61a1a2a43f7ceb2d493fbd2`, equal to development `.lake/build/bin/lean-fmt`. Shadowed `/Users/jcreinhold/.cargo/bin/lean-fmt` differs: `d4cc069608ac2adddb6c93eb35f7a688a970bec514b514906bab65497eec4682`. Hash equality does not identify source revision. |
| Proofs formatter plugin | [Manifest](../../proofs/lake-manifest.json#L14) and dependency checkout agree on `57c2390e5c27afd4e89ff1c7e497bcee6c69addd` (tag v4.35.0), separate from development HEAD and installed CLI. No built plugin was qualified. |
| Configured MCP/old images | Global `.claude.json`, global `.codex/config.toml`, Proofs `.mcp.json` and `.codex/config.toml` point to `http://127.0.0.1:8765/mcp`. Old host/worker files remain under `/Users/jcreinhold/.local/share/lean-host-mcp/releases/0.16.5/`. No supported configuration or fallback may select them after cutover. |

Seven per-toolchain worker installations were inventoried; older sidecars include absent/protocol10 and failed-smoke
claims. No deletion follows from those metadata facts. No Lean-related plist was found among three user LaunchAgents;
that bounded observation does not prove service absence. Read-only `ps` was sandbox-denied; live endpoint identity,
helper population and quiescence remain **unverified**. That inspection was not retried through another route.

Detailed scratch evidence, full image hashes, source inventories, frozen design/spec snapshots and the original hash
manifest remain at `/private/tmp/lean-cutover-inventory-20261117/`. They are supporting observations, not production
dependencies or authorization to operate on installed state.

## Staged acceptance and deletion checklist

All cutover checks below are **pending**. Existing private store/healthy-guardian tests establish only their stated
scope; they do not qualify replacement-service execution, warm integration, arbitrary takeover or deployment.

1. **Select and qualify custody.** Reconcile the SDK custody experiments into the existing-registry recovery contract.
   Qualify frozen guardian, takeover, writer custody and recovery-only demand admission where claimed. State unsupported
   cases explicitly. No more storage/controller layers to defer this decision.
2. **Wire privately and remove duplicate roles.** Implement one scope/service/client path behind an off-by-default
   versioned capability. Preserve original binding, one-use launch, result-before-ACK and independent resource closure.
   Keep warm actor/session reuse; move broker global grants rather than adding another permit. No automatic legacy
   execution fallback on discovery, socket, capability, timeout, SQL or native errors.
3. **Qualify exact disposable images before installation.** Freeze revisions, hashes, SDK lockfile, OS and fixture.
   Failed setup is NOT RUN. Require:
   - [ ] Original job/stage/revision launches at most once through lost CREATE/START/GO/spawn/result replies and restart.
     Consumed uncertain work stays Unknown; GET/ACK never launches; new revision is explicit.
   - [ ] Stale supported launcher, service incarnation/epoch, helper, worker, registry and developer override refuse
     before dispatch; no fallback. Singleton ownership and unchanged canonical lock device/inode are checked.
   - [ ] Actual closure or explicitly qualified recovery precedes conflicting work. Same-inode contender checks payload,
     launchers/custodians and no later writes. Crash/freeze/fork/exec/FD-loss, partial unions and writer demand pass or
     remain stated limitations; timeout, bare EX, saved PID or SQL status is not closure.
   - [ ] Warm sequential attempts share generation custody; request terminal state is distinct from generation retirement.
     One capacity policy remains, with physical occupancy retained until actual closure.
   - [ ] Caller saves verified original result bytes/digest before native ACK; lost ACK and native expiry preserve caller
     copy. SQL failure cannot block cleanup. Storage corruption/assets/pruning retain their separate qualification gates.
   - [ ] Proofs source snapshots, theorem scope and [final complete-input/acceptance checks](../../proofs/scripts/stacks/src/stacks/orchestration/compilation.py#L645)
     remain intact; tooling success never closes a proof claim. Formatter managed-group/no-build validation still passes.
4. **Later authorized maintenance cutover.** Stop submissions at the complete launcher set. Preserve original receipts;
   list accepted/started/unknown attempts. Finish or explicitly cancel through actual owners and observe closure; no
   blind global drain. Preserve native registry and canonical lock inodes. Update exact endpoint owner, explicit paths,
   overrides and matched worker/client images before enabling new submissions. Obtain permitted live-ownership evidence;
   this inventory supplies none. Only after separate authorization run a newly scoped real mixed workflow and record
   implemented/reviewed/private-tested/deployed/production-observed columns.
5. **Delete after parity; list exact deletions and exceptions.** Static searches and runtime refusal tests must
   establish:
   - [ ] Zero migrated Proofs native-helper `Popen`/late-wait owners across all three paths, including artifact read proxy;
     remove `_HelperWaitOwner` only after its last such caller moves.
   - [ ] Zero normal-path client-owned borrowed EX aliases, including SDK `_controller_locks`; audit inherited duplicates.
   - [ ] Zero independent broker/global grant policies outside service; no Python native SQL queue/lease/expiry policy.
   - [ ] One recovery/expiry implementation and one compatibility path; no automatic old-protocol execution branch.
   - [ ] Superseded controller loops, proxy framing/FIFO ownership and inactive compatibility factory removed or made
     service-only adapters. Preserve historical receipt decoding and qualified native mechanisms where useful. Enumerate
     private negative fixtures, header-only parsing, isolated CI and generic SDK/standalone exclusions explicitly.

## Receipts and rollback

Keep [semantic journal recovery](../crates/lean-host-mcp/src/tasks.rs#L155), Proofs
[immutable native history](../../proofs/scripts/stacks/src/stacks/native_history.py#L21),
[GET-only continuation](../../proofs/scripts/stacks/src/stacks/native_client.py#L266) and caller result-before-ACK
order. Current GET/CANCEL require the original request ID; the [epoch check](../crates/lean-host-mcp/src/tasks.rs#L950)
applies when supplied. ACK requires epoch and submission digest; submit checks its saved binding. Do not describe all
current operations as enforcing the same complete mutation identity. Old journals/schema-3 archives/objects remain
readable evidence with original IDs; never import them as runnable native jobs, fabricate old success from newer work,
or require a missing historical receipt after resources are established free.

Rollback pauses **new submissions** and preserves new store, binaries, caller bindings, receipts and native owner facts.
Report which attempts started and their result/resource states. Close or reconcile new-version scopes through their
actual owner before selecting an older qualified authority for new work. Never run competing custody systems, downgrade
live SQLite in place, replay consumed/unknown attempts, rotate locks or erase owner records. If the older image cannot
safely cooperate with retained formats/state, remain paused. Preserve new history with its supported reader or immutable
evidence; rollback does not rewrite the meaning of semantic tickets or proof acceptance.
