# Architecture

This file states what the server must do, who owns each part, and the test that checks each rule. The
[README](../README.md) covers setup, [operations](operations.md) covers running the server, and the
[tool catalog](tool-catalog.md) gives each tool's contract.

## Goal

Many agent sessions on one Lake checkout check, build, look up and search Lean code at the same time through one local
server, with Lake as the only build system.

| Requirement | Meaning | Checked by |
| --- | --- | --- |
| Correct | No check reads build outputs while a build writes them; a check after a build sees its outputs | `tests/many_clients.rs`: `many_clients_share_one_project`; `tests/requests.rs`: `the_next_server_serves_only_after_a_killed_servers_build_exits` |
| Concurrent | Checks from different sessions run at once, up to set limits; a build waits for running checks, then runs alone | `many_clients_share_one_project`; `tests/requests.rs`: `two_file_checks_run_lean_at_once` |
| Warm | Repeated checks on the same imports reuse a loaded worker; only builds and memory limits restart it | `tests/smoke_perf.rs`: `repeated_calls_on_one_import_profile_never_recycle_the_worker`, `alternating_import_profiles_reuse_pooled_sessions_and_never_recycle`; `tests/e2e.rs`: `a_project_under_its_soft_budget_never_cycles_for_residue` |
| Contained | When the server stops or dies, its children stop; the next server serves only after they have exited | `tests/requests.rs`: `the_next_server_serves_only_after_a_killed_servers_build_exits`, `a_killed_servers_workers_exit_and_the_next_build_runs`; `tests/project_workers.rs`: `after_a_kill_during_worker_start_the_next_server_has_every_slot`; `tests/stdio_lifecycle.rs`: `stdio_shutdown_on_stdin_eof_exits_parent` |
| Lake-native | Lake decides what to build, where outputs live and whether they are current; the server reads none of Lake's files | `tests/build_path.rs`: `build_path_never_queries_the_import_graph`. None yet for file reads: the server still reads the files [listed below](#where-the-server-still-reads-lakes-layout), so an `rg` check cannot pass |

Not goals: several servers or worktrees on one checkout; guarding the checkout against Lake commands that bypass the
server; a general-purpose SDK; public tools for raw elaboration, kernel checks or cursor queries; text search. The
worker is not a sandbox: Lean code that a check runs can do IO with the server user's rights.

## Who owns what

| Package | Owns | Never holds |
| --- | --- | --- |
| lean-rs | Running Lean from Rust: load a toolchain, import, elaborate, evaluate; worker children that exit when their parent dies; asking Lake for paths | Locks between processes; a consumer's names or policy; ranking |
| lean-host-mcp | One checkout: the order of checks and builds, the worker pool and its limits, every `lake` and `lean` run, the MCP tools, named requests | Lean runtime mechanics; search logic; answers Lake gives |
| lean-semantic-search | Search: extraction, declaration ids, ranking, its Lean runtime and the driver that runs it | Locks; when to build; reply shapes |
| Lake | What to build and in which order, where outputs live, whether they are current, each file's options |  |
| A client project | Its Lean code; calls the server through MCP | Copies of Lake or server logic |

Put new code in the lowest package that owns its inputs. When one Lake command answers the question, ask Lake. A package
never names its consumers.

### Modules

```
crates/lean-host-mcp/src/
  main.rs                  CLI, broker setup, the stdio entry
  transport_http.rs        the private axum/rmcp Streamable HTTP entry
  server.rs                LeanHostService: tool registration and nothing else
  tools/                   semantic.rs, the facade for the five semantic tools; position, declaration,
                           declaration_inventory, proof_search and proof_action behind it; build.rs for
                           lean_build and lean_imports; source_input.rs reads a client's file once;
                           request_aliases.rs maps request synonyms
  envelope.rs, trust.rs    the internal envelope and its { data, errors, trust } projection; trust facts
  projections.rs           MCP projections of lean-rs worker types
  broker.rs                ProjectBroker: the served root, worker slots, short-run permits, idle retirement
  broker/control.rs        activity snapshots for lean_runtime
  broker/host_artifacts.rs the installed shims a worker reads before it opens
  broker/module_setup.rs   every Lake run that reads the project: setup-file and query
  broker/reader.rs         the permit, read share and exit codes of each short run a check starts
  broker/verify_file.rs    file checks through the Lean drivers in lake/
  broker/semantic_file.rs  proof search through lean-semantic-search's driver
  project.rs               LeanProject actor: one controller thread and one worker child
  project/                 runtime_config.rs, the actor's limits; restart.rs, restart causes and counts;
                           semantic_runtime.rs, the search runtime install builds and a server opens
  worker_retirement.rs     keeps an actor's slot until each child it tracked has exited
  project_lock.rs          read and write turns per Lake root, and the read shares of short runs
  build.rs, build/         lean_build: the [build] and [recipes] config, word checks, steps, the capped log
  lean_run.rs              short runs: one lean or lake program in its own process group, output capped
  store.rs, children.rs    the store and server.lock; children.lock and the child files
  requests/                named requests: the SQLite table, one launch per id, recovery at start
  cache.rs                 the module-query cache, valid while its worker generation lives
  ilean.rs                 a reader for Lean's .ilean indexes
  lake_meta.rs             the Lake root, its toolchain pin, a manifest fingerprint, the manifest's roots
  toolchain.rs             the pin's elan toolchain and worker, and the readiness gate
  host_preparation.rs      the worker's readiness mode, and the preparation install runs
  runtime_compatibility.rs the host bindings a worker session must have
  semantic_search.rs       the adapter for the lean-semantic-search runtime
  config_file.rs           the tracked and local config files at the Lake root
  config_schema.rs         every setting in one table
  cli/                     install, install-worker, config init, doctor processes
  status.rs                build identity and resolved settings for lean_status
  diagnosis.rs             needs_build, unknown_import and the other recovery messages
  smoke.rs                 the install smoke test and its verdict
  request_log.rs           content-free summaries of each call
  cancellation.rs          cancellation shared by async callers and the worker owner
  error.rs, digest.rs      the handlers' one error type; SHA-256 as lowercase hex

crates/lean-host-mcp-worker/src/
  lib.rs                   worker.json, the deployment record both sides check; no Lean
  main.rs, authority.rs    the worker child: serves one project's build through its launch roots
```

## One server per checkout

One server serves a Lake checkout and starts every Lake and Lean run on it, so it can order checks and builds in memory.
Two `flock` locks in the store hold this across processes. The kernel frees a flock only when the last process that
shares its open file description exits, however it exits. Nothing in the server unlocks either one: an unlock would free
the lock for every process that shares it.

`server.lock` admits one server per store. The server takes it exclusively and close-on-exec, and writes its pid and
Lake root into the file; a second server exits at startup and names the first. The lock binds the store, not the
checkout: two servers that use different stores for one Lake root are outside the contract. `install` and
`install-worker --lake-root` take the store the same way (`store::hold`), so each refuses while a server serves the
root.

`children.lock` outlives the server. Once it holds `server.lock`, the server takes `children.lock` exclusively and
clears its close-on-exec flag, so every child it starts, and their children, share the lock. Before a build step or a
short run, the server creates a file `starting-…` in the store's `children/`, locks it, and passes that descriptor to
the child too (`process_group::spawn_inheriting`). Once the child runs, the server renames the file to the child's
process group, and it deletes the file once it has seen the group exit. A held file lock means a process of that group
still runs; a free one means none does. Workers and the Lake run that opens the search runtime have no file, since
lean-rs and the search crate start them. They hold `children.lock` like every child, and a worker exits with status 125
once the pipe to its server closes.

At startup, after `server.lock`, the server takes `children.lock` (`children.rs`):

- If it is free, no process of an earlier server runs. The server deletes every file in `children/` and serves.
- If it is held, the server sends SIGTERM to each recorded group whose file lock is held, waits up to 5 seconds, then
  sends SIGKILL to each group still held. It deletes each file whose lock is free, waits up to 30 seconds for
  `children.lock`, and serves once it has it.
- If it is still held, the server exits with an error that names the groups still running, says that processes it does
  not record, such as a worker whose pipe has not closed yet, may hold the lock too, and that
  `lsof <store>/children.lock` lists every holder. [Operations](operations.md#a-refused-start) gives the steps.

The server never signals a group whose file lock is free and never looks for processes by name. A process that closed
the inherited descriptor can outlive the wait, and one that left its group escapes the signals. If every member left a
group while one still holds the file's lock, the group number can name another group by the time the signal goes.

| Invariant | Test |
| --- | --- |
| A second server on one store exits at startup, names the first, and leaves the first's children alone. | `store.rs`: `second_lock_names_the_holder`, `empty_record_still_refuses`, `a_refused_start_names_the_holder_and_leaves_children_alone`; `tests/http_transport.rs`: `second_server_on_one_store_names_the_first` |
| The store is private to the server's user and ignored by Git. | `store.rs`: `store_is_private_and_ignored` |
| The next server serves only after the last one's children exit: it stops each recorded group whose file lock is held, then waits for `children.lock`. | `children.rs`: `a_held_child_file_gets_its_group_stopped_and_the_lock_frees`, `a_free_children_lock_clears_stale_files_and_passes_to_children`; `tests/requests.rs`: `the_next_server_serves_only_after_a_killed_servers_build_exits`, `a_killed_servers_workers_exit_and_the_next_build_runs` |
| A server never signals a group whose file lock is free. | `children.rs`: `a_child_file_whose_lock_is_free_is_never_signalled`, `stop_group_kills_members_a_leader_left_only_while_one_holds_the_lock` |
| A server killed while a worker starts takes no slot from the next server. | `tests/project_workers.rs`: `after_a_kill_during_worker_start_the_next_server_has_every_slot` |
| `install` refuses while a server serves the root, and writes nothing. | `cli/install.rs`: `install_refuses_while_a_server_serves_the_root` |

## Turns and builds

`project_lock.rs` gives each canonical Lake root one fair `tokio::sync::RwLock`. A check takes a read turn and a build
takes the write turn. Tokio grants turns in arrival order, so a queued build holds back later checks, and each waiter
keeps its place until its deadline. A request takes at most one turn and keeps it in a task-local slot: a nested check
in the same request reuses its read turn instead of queueing behind a waiting build, and a request that holds a read
turn cannot take the write turn. A check takes its read turn before it pins a worker or starts a short run and holds it
until the request ends. A short run whose process group outlives its request keeps a share of the turn
(`project_lock::read_share`) until the group exits. Each finished write turn adds one to the root's finished-build
count.

Every Lake run that reads the project starts in `lake_reader` (`broker/module_setup.rs`), which takes the request's read
turn first. So no such run overlaps a build, and the check it serves keeps the same turn.

`lean_build` (`build.rs`, `tools/build.rs`) plans the run from the `[build]` and `[recipes]` config and refuses a
request that does not fit it. It takes the write turn, stops the project's workers and waits until each has exited
(`ProjectBroker::stop_workers`), then runs the guard and each step as `lake <args>` in the Lake root. Each step runs in
its own process group with its own child file, and all output goes to one capped log in the store. Cancellation or the
build's deadline stops the running step's group, and the turn ends only after each step's group has exited. A build that
changes nothing still stops the workers. Lake decides what to build and in which order; the server checks only the words
it hands to Lake and walks no import graph.

Lean writes each `.olean` to a temporary file and renames it into place (`src/library/module.cpp` in Lean), so a reader
never sees a torn file and a file a worker has mapped never changes under it. The turns prevent the one hazard left: a
check that loads some modules from before a build and some from after.

After a lakefile edit, the next `lake setup-file` rewrites Lake's configuration cache, and two checks can start that
rewrite at once. Lake locks the cache itself (`importConfigFile` in `Lake/Load/Lean/Elab.lean`), so it stays whole. Of
two rewrites that start together, Lake fails one with "could not acquire an exclusive configuration lock"; that check
reports Lake's error, and a retry reads the new cache.

The turns order only the runs this server starts. A `lake build` run outside the server can rewrite `.olean` files while
a check reads them, and neither the turns nor the finished-build count see it.

| Invariant | Test |
| --- | --- |
| Checks on one project hold read turns at once, whichever clients send them; a build waits for every read turn and share, then runs alone. | `project_lock/tests.rs`: `two_requests_hold_read_turns_at_once`, `a_read_share_keeps_a_build_waiting_after_its_request_ends`; `tests/requests.rs`: `two_file_checks_run_lean_at_once` |
| Turns go in arrival order, and a check queued behind a build sees what it built. | `project_lock/tests.rs`: `builds_and_checks_take_turns_in_arrival_order`, `a_queued_writer_goes_before_a_later_reader_which_sees_its_finish`; `tests/build_requests.rs`: `builds_run_one_at_a_time_in_arrival_order`, `a_check_sent_during_a_build_waits_and_sees_what_it_built` |
| No build starts between a check's `setup-file` and its use of the imports, and a check cannot build. | `project_lock/tests.rs`: `a_nested_read_reuses_the_turn_while_a_writer_waits`, `a_check_cannot_build` |
| A wait for a turn ends at its deadline or on cancel, and leaves the line. | `project_lock/tests.rs`: `a_read_wait_ends_at_its_deadline_or_on_cancel`, `a_write_wait_ends_at_its_deadline_or_on_cancel_and_leaves_the_line` |
| No Lake run that reads the project overlaps a build. | `broker/module_setup.rs`: `every_lake_run_waits_for_a_build` |
| A build stops the project's workers, and each has exited before Lake starts; a check sent during a build answers from the new build. | `tests/project_workers.rs`: `a_build_stops_the_project_workers_before_lake_starts`, `a_check_sent_during_a_build_waits_and_answers_from_the_new_build` |
| No step leaves a process running past its build, and the deadline stops a build and frees the project. | `tests/build_requests.rs`: `a_step_cannot_leave_work_running_past_its_build`, `the_deadline_stops_a_build_and_frees_the_project` |
| A stale guard target stops the build before Lake writes; a failing step ends the build, and the reply carries the end of the log. | `tests/build_requests.rs`: `a_stale_guard_stops_the_build_before_lake_writes`, `a_failing_step_ends_the_build_and_its_tail_shows_the_error`, `a_large_output_completes_and_the_reply_carries_its_end`; `build/tests.rs`: `recipes_place_the_guard_and_the_args` |
| A build request with a word Lake would read as an option, or a recipe it does not fit, is refused. | `build/tests.rs`: `requests_that_do_not_fit_are_refused` |
| The build log keeps a capped prefix and one truncation line, and refuses a symbolic link. | `build/log.rs`: `the_cap_keeps_a_prefix_then_one_truncation_line_across_steps`, `a_log_path_that_is_a_symbolic_link_is_refused` |
| The build path walks no import graph. | `tests/build_path.rs`: `build_path_never_queries_the_import_graph` |
| After a lakefile edit, checks leave the configuration cache to Lake. | `tests/build_requests.rs`: `reads_after_a_lakefile_edit_leave_the_config_cache_to_lake` |

## Workers

`ProjectBroker` (`broker.rs`) serves one canonical Lake root and refuses a call that names another before it asks Lake
for options. Each `LeanProject` actor (`project.rs`) has one controller thread, one worker child, its own cache and
restart state, and one job at a time. Its mailbox is bounded; admission is in arrival order, can be cancelled, and ends
at the request's deadline; a full mailbox returns retryable pressure. The parent never links `libleanshared`; the worker
child for the project's toolchain does.

The broker admits up to `max_workers_per_project` actors within `max_workers` slots. It reserves a slot before it opens
a worker, and an actor holds its slot while it opens, serves and retires, until every child it tracked has exited
(`worker_retirement.rs`); cleanup it cannot confirm keeps the slot. Idle retirement skips an actor that a call holds or
that has queued work. The cap counts actors, not bytes. Short runs have their own bound.

**Readiness.** Before it spawns a worker, `WorkerBinary::resolve_ready` (`toolchain.rs`) checks the supported-toolchain
table, the elan toolchain, the generation that `current-generation` selects in the project's worker directory, and that
generation's `worker.json`:

| Verdict | Meaning |
| --- | --- |
| `Unsupported` | The pin is not in the supported table; reported before any file probe, with the nearest supported version |
| `ToolchainNotInstalled`, `NotInstalled` | No elan toolchain, or no worker in the selected generation |
| `Stale` | The toolchain's `lean.h` differs from the digest in the record |
| `Incompatible` | The record's worker protocol differs from this server's |
| `Unusable` | The record does not load, holds no passing smoke test, or names another generation |
| `UnknownPin` | A pin that is not a version number; allowed with a warning |
| `Ready` | May spawn |

A hard verdict refuses before spawn with one line that names the worker directory and the fix,
`SERVER install --lake-root PROJECT`. A worker that another server version built serves when its protocol matches. Every
new or replacement session checks that the worker has each host binding it needs before it serves
(`runtime_compatibility.rs`). Before it takes a slot, the broker asks the worker's readiness mode for the descriptor of
the shims `install` built (`broker/host_artifacts.rs`); missing or stale shims fail the open with the install command,
and the server never builds them.

`worker.json` (`crates/lean-host-mcp-worker/src/lib.rs`) records the toolchain, the worker's digest, the Lean prefix,
the shim roots and the digest of each file the worker loads, the `lean.h` digest, the protocol and the smoke verdict.
Install writes it without a verdict, runs the smoke test (import `Init`, inspect `Nat.add_zero`), records `passed`, and
only then selects the generation; a failure leaves the earlier selection in place. Before each file check the server
hashes the shim files again and finds the `.olean` Lean will load for each driver module by Lean's `findOLean` rule over
the recorded roots (`broker/verify_file.rs`).

**Import roots.** The worker reads the project's own build in place. At launch the broker names the import roots in
Lean's search order, from the Lake manifest (`lake_meta::manifest_artifact_roots`): the root package's
`.lake/build/lib/lean`, then each manifest package's. The worker admits an import only when the request's roots are
exactly the launch roots, in order, then the shim roots, and no root holds a package whose compiled code the worker
runs, such as `Init`. No lock guards these roots: only the server that launched the worker builds the checkout, and it
stops the worker first. The server copies no files and hashes no build outputs.

**Session choice.** A tool that reads a client's file picks the worker session by the file's imports. `read_query_file`
(`tools/source_input.rs`) takes them from a scan of the header in the same bytes it hashes and sends. The scan reads the
header as Lean does: it skips whitespace, `--` comments and nested `/- -/` comments, and stops at a doc comment or at
any word but `module`, `prelude` and an `import`. The server scans because lean-rs parses a header only inside a session
that is already open. When the scan misses an import that Lean finds, the worker reports it missing and the reply asks
for a build.

**Memory.** After the first import, each import copies its whole closure into private memory: 2.0–4.5 GB each on large
headers that import Mathlib. The `non_memory_mapped_region_bytes` the worker reports, summed, tracks the child's
physical footprint (slope 1.09, R² 0.96), where RSS and import counts do not. So that sum, against
`runtime.worker_import_residue_budget_mib`, decides when to recycle a child, and RSS is only reported. Lean's heap
ceiling is a backstop above every residue budget, since crossing it aborts the child with status 134
([sizing](operations.md#sizing)). The session pool keeps outgoing environments apart from the budget, since keeping one
costs about 70 MiB and importing it again costs gigabytes. `max_imports` (32) backs up statistics that read zero. Above
60% of the budget, a quiet actor cycles as `import_residue_idle` after two seconds, at most once per quiet period, and
prewarms the latest profile if it served it within a minute. Planned cycles do not count toward the actor's crash-loop
breaker (`project/restart.rs`).

**Rebuilt outputs.** A pooled environment is a snapshot of its imports, so no build may change the files under a live
worker, and builds stop workers first. As a backstop, each job carries the finished-build count of its read turn; a
count above the one under which the live generation first loaded imports cycles the worker before the job as
`artifacts_rebuilt` and drops every pooled profile.

**No replay.** Lean elaboration and tactics may do IO, so a worker's death never permits a replay. The opt-in
`retry_tainted_non_positive` policy allows one retry of a non-positive trial or verification verdict after a mid-call
recycle. The reply keeps the earlier failure apart and carries a warning, and a tainted or interrupted final attempt is
never cached.

**Receipts.** After each completed call the actor checks the host facts the child reported and keeps a receipt bound to
its session and worker generation while that generation lives; `lean_status` reads it without asking a worker.
`lean_runtime` reads the pool under the broker's registry mutex and changes nothing; startup, queued jobs, preparation
and background cycling all count as activity (`broker/control.rs`).

| Invariant | Test |
| --- | --- |
| Workers stay within `max_workers` and `max_workers_per_project` while checks, trials and builds share a project. | `tests/many_clients.rs`: `many_clients_share_one_project`; `broker.rs`: `same_project_workers_share_capacity_and_stop_together` |
| Slot waiters go in arrival order, and a cancelled waiter takes no slot. | `broker.rs`: `capacity_waiters_are_fifo_and_cancelled_waiters_do_not_steal_slots` |
| No slot returns while its worker may live. | `worker_retirement.rs`: `late_retirement_waits_for_ordinary_and_companion_child_and_releases_once`, `cleanup_thread_creation_failure_keeps_tracker_and_permit_for_later_poll`; `broker.rs`: `shutdown_interrupts_retirement_wait_without_unconfirmed_slot_release`; lean-rs `lean-rs-worker-parent/tests/worker_shutdown.rs` |
| A call that names another root, or a root without a lakefile, is refused before Lake runs. | `broker.rs`: `native_calls_refuse_every_root_but_the_served_one_before_options`; `tests/resolution.rs`: `explicit_hint_naming_another_root_is_refused`, `a_broker_without_a_root_refuses_every_hint`, `a_root_without_a_lakefile_is_not_a_lake_project` |
| A worker opens only from a ready generation, and each hard verdict names the install command. | `toolchain.rs`: `out_of_window_pin_is_unsupported_before_install_check`, `protocol_mismatch_is_a_hard_incompatibility`, `forged_mismatching_digest_is_stale`, `a_record_without_a_smoke_verdict_is_unusable`, `layouts_an_older_release_made_are_not_installed`, `package_version_skew_is_ready_when_protocol_is_compatible`, `install_fix_is_one_shell_command`, `missing_worker_names_the_toolchain_and_directory`; `server.rs`: `incompatible_worker_is_nonretryable_with_exact_recovery_command` |
| A session serves only when the worker has every host binding it needs. | `runtime_compatibility.rs`: `missing_or_incompatible_binding_prevents_body_execution` |
| Missing shims fail the open with the install command and build nothing; a file check loads only the shim files the record names. | `broker/host_artifacts/tests.rs`: `missing_shims_fail_with_the_install_command_and_build_nothing`; `broker/verify_file.rs`: `shims_validate_only_with_the_recorded_olean_of_each_driver_module`, `a_shim_module_loads_from_the_first_root_that_holds_its_package` |
| The worker record keeps paths relative to its generation, stays under 1 MiB, and resolves no path outside the shims. | `crates/lean-host-mcp-worker/src/lib.rs`: `the_record_stores_generation_paths_relative_and_read_resolves_them`, `read_refuses_a_record_over_one_mebibyte`, `resolve_in_refuses_a_path_that_leaves_the_shims` |
| The parent binary never links `libleanshared`. | `scripts/check-parent-link.sh`, run by CI's "Check the parent link set" step |
| A worker serves trials, proof positions, lookups and checks of modules without a source file from the project's own build, path dependencies included. | `tests/project_workers.rs`: `a_trial_imports_a_dependency_module_and_prints_its_axioms`, `a_proof_position_and_a_lookup_answer_from_the_project_build`, `a_module_without_a_source_file_is_checked_in_a_worker` |
| A worker admits an import only through its launch roots, in order, then the shim roots, while no root holds `Init`; the server refuses to start with `LEAN_PATH` set. | `tests/project_workers.rs`: `an_import_through_other_roots_is_refused`, `an_import_root_that_holds_init_is_refused`; `tests/http_transport.rs`: `an_inherited_lean_path_refuses_startup` |
| The header scan finds the imports Lean's parser finds, from the bytes the server hashes. | `tools/source_input.rs`: `header_imports_handles_public_meta_and_all`, `header_imports_skips_a_copyright_block_before_module`, `header_imports_skips_a_comment_inside_a_line`, `header_imports_skips_nested_block_comments`, `header_imports_stops_at_the_first_token_after_the_header`; `tools/position.rs`: `read_query_file_imports_are_the_header_scan_of_its_source` |
| The heap ceiling stays above every residue budget, and the pool size and the budget are set apart. | `project/runtime_config.rs`: `the_heap_ceiling_stays_above_every_residue_budget_the_resolver_can_produce`, `pool_capacity_and_the_residue_budget_are_independent_knobs`, `the_residue_budget_defaults_to_one_childs_share_of_this_machine` |
| Repeated calls on held import profiles never recycle the worker; a project under its soft budget never cycles for residue, and an idle one over it cycles between calls. | `tests/smoke_perf.rs`: `repeated_calls_on_one_import_profile_never_recycle_the_worker`, `alternating_import_profiles_reuse_pooled_sessions_and_never_recycle`; `tests/e2e.rs`: `a_project_under_its_soft_budget_never_cycles_for_residue`, `an_idle_project_over_its_soft_budget_cycles_between_calls` |
| A sweep of changed files recycles only on its residue budget, and the recycle comes before Lean's heap ceiling aborts the child. | `tests/kanproofs_eval.rs`: `a_changed_file_sweep_recycles_only_on_its_residue_budget`, `the_residue_recycle_reaches_the_child_before_the_heap_ceiling_aborts_it` |
| Lifecycle polls keep healthy idle workers warm without health calls and cycle a quiet actor at most once per quiet period. | `project/receipt_tests.rs`: `lifecycle_polls_keep_healthy_idle_workers_warm_without_health_rpcs`, `lifecycle_ticks_preserve_residue_quiescence_and_one_cycle_per_quiet_period` |
| Planned cycles do not count toward the abnormal-restart limit. | `project/restart.rs`: `planned_restart_causes_do_not_consume_abnormal_restart_budget` |
| A job whose read turn follows a newer build replaces the worker once, and an unchanged query sees a rebuilt dependency. | `project/receipt_tests.rs`: `a_build_finished_since_load_replaces_the_worker_once`; `tests/artifact_invalidation.rs`: `unchanged_query_sees_a_rebuilt_dependency` |
| A worker's death after a call took effect never replays the proof attempt; only an explicit, completed retry clears an earlier failure. | `project/receipt_tests.rs`: `abrupt_worker_exit_after_effect_never_replays_actual_proof_attempt`; `tools/proof_action.rs`: `retry_tainted_non_positive_defaults_to_false_and_parses_absent_field`, `taint_retry_surfaces_through_retry_count`; `diagnosis.rs`: `only_explicit_completed_retry_clears_previous_failure_not_final_restart` |
| A completed call leaves a receipt that `lean_status` reads without opening a worker; a replaced worker drops it; check replies carry none. | `project/receipt_tests.rs`: `a_completed_call_leaves_a_receipt_for_status`, `a_replaced_worker_drops_the_old_receipt`; `tools/semantic.rs`: `lean_status_does_not_open_worker_and_reports_trust_artifacts`, `check_replies_carry_no_compatibility_record` |

## Short runs

A short run (`lean_run.rs`) is one `lean` or `lake` program in its own process group, with capped output and its own
child file. Each file check (`broker/verify_file.rs`, through the drivers in `lake/`), each Lake question
(`broker/module_setup.rs`) and each proof search (`broker/semantic_file.rs`) is a short run. Each takes a permit from
the broker's `broker.max_lean_runs` semaphore before it starts, and keeps the permit and a share of the request's read
turn until its group has exited, even past its reply (`broker/reader.rs`). A run that gets no permit by its deadline
never starts. Short runs count as activity but take no worker slot, so they never evict a warm worker. A run whose group
outlives its stop reports an unknown outcome, never a run that did not start.

A file check reports what `lake build` reports, and an exit code that disagrees with the messages decides nothing.

| Invariant | Test |
| --- | --- |
| At most `broker.max_lean_runs` short runs at once; a run that gets no permit by its deadline never starts. | `broker/reader.rs`: `a_run_that_gets_no_permit_by_its_deadline_never_starts`; `tests/requests.rs`: `with_one_lean_run_two_file_checks_take_turns`, `two_file_checks_run_lean_at_once` |
| A cancelled run writes no inputs and never starts. | `lean_run.rs`: `a_cancelled_run_writes_no_inputs_and_never_starts`; `broker/reader.rs`: `pre_cancelled_reader_admission_records_no_start_without_preparing_inputs` |
| A run stops its whole group at its time limit, past its output cap, or when its leader leaves members running, and its reply never waits on a process outside the group. | `lean_run.rs`: `a_run_past_its_limit_stops_the_group`, `output_past_the_cap_stops_the_group`, `a_leftover_member_holding_stdout_open_is_stopped_without_waiting_for_eof` |
| A short run's reply names a session of its own. | `broker.rs`: `short_run_replies_name_a_session_of_their_own` |
| A file check reports what `lake build` reports. | `tests/e2e.rs`: `lean_trial_command_and_lean_status_file_diagnostics`, `worker_file_checks_see_the_header_and_drop_silent_messages`; `broker/verify_file.rs`: `an_exit_code_that_disagrees_with_the_messages_decides_nothing`; proof search's goal elaboration is checked by lean-semantic-search's `LeanSemanticSearchTest.lean` |

## Lake's answers

The server asks Lake each question Lake can answer. Every run goes through `lake_reader` as
`lake --no-build --no-cache ARGS`, with Lake's cache pointed at the run's private directory under the store's `runs/`:

| Question | Lake run |
| --- | --- |
| A file's options | `setup-file FILE '{"imports":[],"isModule":false}'`; Lake reads the second argument as the header (`setupServerModule` in `Lake/Build/Module.lean`), so it checks no import |
| A file's imports, and whether they are built | `setup-file FILE`; exit 3 lists the targets that need a build |
| Whether a failed import names a module | `query +MODULE:lean`, for at most 8 names per failure |
| Module sources and direct imports | `query --json +MODULE:lean +MODULE:header` |

**Options.** Lake answers as it answers an editor. A file in a library, a dependency's library included, gets that
library's server options (`LeanLib.serverOptions`): the build type's options, the package's `leanOptions` and
`moreServerOptions`, then the library's. A file outside every library, and a label such as `<elaborate>` that names no
file, get the root package's (`Workspace.serverOptions`). `-D` arguments in `moreLeanArgs` and `weakLeanArgs` are not
server options, so checks do not get them. Lake still fetches what the library needs, its `extraDep` target, dynlibs and
plugins: when one needs a build, Lake exits 3 and the check replies `needs_build` with Lake's targets. Any other failure
stops the check before it starts, with `target_check.stage` set to `lake_option_discovery`; a lookup that gets no read
turn by its deadline replies `project_admission_timeout`. The server never retries with default options. The worker gets
typed options, `weak.*` included, and options are part of the cache key, so a lookup runs before every source check,
cache hits included; it takes about 0.4 s on the test fixture. The internal envelope's `freshness.elaboration_options`
records the options and the host's heartbeat limit, which overrides the project's `maxHeartbeats`.

**Imports.** A check builds nothing; the [tool catalog](tool-catalog.md#response-shape) gives the `needs_build` and
`unknown_import` rows it returns instead. `lake query +MODULE:lean` tells an import that names no module from a module
Lake knows but has not built. `lean_imports` reads each file's import closure from `lake setup-file` and the sources and
headers of modules from the queries above.

### Where the server still reads Lake's layout

- `lake_meta.rs` reads `lake-manifest.json` for the project's fingerprint, for the worker's and the proof-search
  reader's import roots, and for the package roots that tell first-party modules from dependencies.
- `ilean.rs` reads the indexes under the root package's `.lake/build/lib/lean`.
- The declaration outline compares one module's source and index modification times ([below](#reference-indices)).

| Invariant | Test |
| --- | --- |
| A check gets the server options Lake gives its file, and they reach diagnostics, verification and the cache key. | `broker/module_setup.rs`: `options_are_the_server_options_lake_gives_each_file`; `tests/e2e.rs`: `lake_options_reach_diagnostics_verification_and_cache` |
| A failed option lookup says no check ran. | `broker/module_setup.rs`: `a_failed_option_lookup_says_the_check_never_started` |
| A check reads the module inputs and headers Lake reads. | `broker/module_setup.rs`: `module_inputs_are_what_lake_reads`, `a_header_parses_as_lake_prints_it` |
| A file check with an out-of-date import builds nothing and replies `needs_build`, naming each target Lake lists and the `lean_build` call that builds it. | `broker/module_setup.rs`: `no_build_exit_names_the_targets_lake_lists`; `diagnosis.rs`: `out_of_date_setup_is_needs_build_with_one_stale_row_per_target`; `tests/build_requests.rs`: `a_file_check_with_a_stale_import_names_the_module_to_build` |
| An import that names no module gets an `unknown_import` row, a module Lake knows but has not built gets a build row, and no reply says to run `lake`. | `broker/module_setup.rs`: `a_failed_setup_names_its_module_jobs_and_bad_imports_once_each`, `only_a_missing_name_or_source_names_no_module`; `diagnosis.rs`: `unknown_imports_say_to_fix_the_import_with_one_row_each`, `unloaded_modules_read_the_module_lean_could_not_load`, `needs_build_text_names_lean_build_not_lake_and_avoids_the_word_ambiguous`; `tests/build_requests.rs`: `a_file_check_of_an_import_that_names_no_module_says_to_fix_it`, `a_trial_tells_an_import_that_names_no_module_from_an_unbuilt_one` |
| `lean_imports` lists what Lake reports, sources under a library's `srcDir` included, and names the file whose import is stale. | `tests/build_requests.rs`: `imports_list_the_whole_closure_with_first_party_sources`, `imports_name_sources_under_a_library_src_dir`, `imports_of_a_file_with_a_stale_import_are_an_error_naming_the_file`, `imports_of_modules_list_each_header_and_the_source_lake_reads` |

## Reference indices

`ilean.rs` reads the `.ilean` indexes that `lake build` writes, without a worker. Project-scope
`lean_lookup(kind="references")` reads them instead of elaborating every source, and the declaration outline adds an
index's declarations to a source outline. The reader accepts only version 5 (`Lean/Server/References.lean`); its module
docs give the schema. Callers hold the project's read turn, so no build writes an index while it is read.

A reference reply reads every index in the root package's build directory, skipping `.lake/packages/` and counting each
index it cannot read, then asks Lake for the sources of the modules with hits in one `lake query`. It makes no freshness
check: `lake build --no-build` on the modules with hits walks each one's import closure, which costs as much as the
lookup or more, 1–4 s per reply on a 5,000-module project.

The declaration outline still compares one module's source and index modification times before it adds the index's rows,
since those rows carry spans in the source that was built. A stale index costs only those rows, and Lake's check would
add about 1 s.

`benches/ilean_reference_scan.rs` times the scan: about 120 ms for a whole-project query over 1,431 modules (76 MB of
indexes). Parsing is about 95% of the cost, so the reader parses each index once and keeps only matching entries. The
scan runs serially under `spawn_blocking`; revisit that if a real scan passes one second.

| Invariant | Test |
| --- | --- |
| A lookup finds definitions and uses across modules and skips local variables and other names. | `ilean.rs`: `resolves_def_and_usages_across_modules`, `ignores_fvar_keys_and_mismatched_consts`, `a_module_without_hits_forms_no_group`, `null_definition_yields_only_usages` |
| An unbuilt project, a missing index and a bad index each get their own outcome, and a bad index is skipped and counted. | `ilean.rs`: `unbuilt_project_reports_not_built`, `malformed_file_is_skipped_not_fatal`, `declarations_in_module_reports_missing_module_index`, `declarations_in_module_reports_project_not_built` |
| Only version 5 parses, wherever the version field sits. | `ilean.rs`: `unknown_version_is_rejected_with_typed_error`, `version_may_be_the_last_field`, `future_version_with_unreadable_shape_is_unsupported`, `missing_version_is_a_json_error` |
| Coordinate arrays have exact lengths, and each part of an index parses alone. | `ilean.rs`: `location_array_arity_is_bounded`, `declaration_info_array_arity_is_exact`, `projections_are_independent`, `declarations_in_module_reads_decl_ranges_from_ilean` |
| A reference reply names the source Lake reads, leaves out modules Lake names no source for, and says it comes from the last build. | `tools/position.rs`: `project_references_name_the_file_lake_reads`, `project_references_leave_out_modules_lake_names_no_source_for`, `project_references_say_they_come_from_the_last_build` |
| The outline drops the rows of an index older than its source. | `ilean.rs`: `a_source_edited_after_its_index_is_newer` |

## Named requests

A named request (`requests/`) is a tool call with a `request_id`; the [tool catalog](tool-catalog.md#named-requests)
gives the id rule, the states, what a restart does and the error codes. `RequestRunner::run` checks the id and
arguments, plans the root, kind and deadline, then inserts the row in a `BEGIN IMMEDIATE` transaction that returns the
stored row when the id exists, so of two calls with one id only one inserts and starts. A check takes its read turn,
commits `mark_running`, then calls the tool. A build commits it in its `OnRunning` hook, once it holds the write turn
and its workers have exited, before its first step. Nothing starts before its row says `running`, and a trigger refuses
any change to a row in a final state. The table is `requests.sqlite3` in the store; `requests/recover.rs` ends or starts
the rows an earlier server left, once the server holds the store.

| Invariant | Test |
| --- | --- |
| One launch per id. | `requests/store_tests.rs`: `parallel_inserts_of_one_id_insert_once`, `one_id_holds_one_call`; `tests/requests.rs`: `the_same_build_sent_three_times_at_once_starts_one_lake` |
| A row exists before launch; only a queued row starts, and only an unended row ends. | `requests/store_tests.rs`: `only_a_queued_row_starts_and_only_an_unended_row_ends` |
| The same id with other arguments is refused and makes no row. | `tests/requests.rs`: `another_call_on_an_id_fails_and_makes_no_row` |
| A stored result never changes. | `requests/store_tests.rs`: `a_final_row_never_changes`; `tests/requests.rs`: `a_named_request_runs_once_and_keeps_its_result` |
| A build outlives its killed client; a restart ends each running request, runs each queued one, and keeps ended ones. | `tests/requests.rs`: `a_build_outlives_its_killed_client`, `a_killed_server_interrupts_its_build_and_runs_the_queued_one`, `an_ended_request_survives_a_server_kill` |
| A call without an id runs with a server id and no row. | `tests/requests.rs`: `a_direct_verify_names_a_server_id`; `requests/tests.rs`: `ids_follow_the_rule_and_srv_ids_belong_to_the_server` |
| The table is private and synced, keeps its rows across a reopen, refuses a newer schema, and prunes no unended row. | `requests/store_tests.rs`: `open_uses_a_private_synced_write_ahead_log`, `a_reopened_table_keeps_its_rows_and_refuses_a_newer_schema`, `prune_drops_old_rows_then_the_oldest_past_the_cap_and_keeps_unended_rows` |
| A request's intent does not depend on key order, every reply carries its id at the top, and a refusal is a tool error. | `requests/tests.rs`: `intent_sorts_keys_at_every_level`, `every_reply_carries_its_id_at_the_top`, `a_refusal_is_a_tool_error_the_client_can_read` |
| Install and serve keep no state outside the store. | `tests/http_transport.rs`: `install_and_serve_keep_no_state_in_an_empty_home` |

## Deadlines and cancellation

`runtime.request_timeout_millis` sets one absolute deadline per request, from the option lookup and project admission
through opening, mailbox admission and the wait behind earlier jobs. Worker operation timeouts are separate. An expired
queued job returns `queue_wait_timeout` and never runs: the actor and the caller share one start-or-expire decision.
Once a job starts, the worker's limits apply. Cancelling a running job waits for the worker's cleanup reply. Dropping
the caller's future also cancels, and the actor keeps the slot until cleanup ends. Neither counts as a passing check.
`project_admission_timeout` or `request_cancelled` from admission means the actor operation never started, though the
option lookup may have run.

| Invariant | Test |
| --- | --- |
| An expired queued job never starts, only one of expiry and start wins, and a started job does not expire in the queue. | `project/tests.rs`: `expired_queue_entry_never_starts`, `queue_expiry_race_has_only_one_winner`, `started_job_is_not_expired_by_queue_deadline` |
| The queue deadline covers both admission and the wait for the actor. | `project/tests.rs`: `queue_deadline_includes_both_admission_and_actor_wait` |
| Mailbox admission goes in arrival order, and a cancelled waiter removes only itself. | `project/tests.rs`: `mailbox_admission_is_fifo_and_cancellation_removes_only_its_waiter` |
| Cancelling a running job waits for the actor's cleanup, and a dropped HTTP call stops Lean. | `project/tests.rs`: `cancellation_of_running_message_waits_for_actor_cleanup`; `tests/http_transport.rs`: `streamable_http_dropped_running_call_stops_lean_and_drains` |

## Cache

`ModuleQueryCache` (`cache.rs`) keeps bounded answers to file queries. A key holds the file path, the source bytes, the
query and selector shape, the output budgets, the imports, the elaboration options and the finished-build count of the
request's read turn. Each entry names the worker generation that computed it and answers only while that generation
lives; there is no TTL. A tainted call inserts nothing, and unknown query kinds skip the cache. A hit skips the actor
mailbox but still runs the option lookup. Name-only lookups, proof attempts and verification are never cached: their
meaning depends on the imported environment and on execution policy, not on the source alone. Worker APIs cap each field
and the whole output before IPC, and the host caps candidates and reference fan-out.

| Invariant | Test |
| --- | --- |
| An entry from a retired generation misses, and so does an entry from before a finished build. | `cache.rs`: `a_retired_generations_cache_entries_miss`, `a_finished_build_misses_unchanged_source` |
| The key holds the options, the imports and the build count, and one file's answer never serves another question. | `cache.rs`: `query_context_includes_options_imports_and_build_count`, `one_files_answer_is_never_served_for_a_different_question` |
| An interrupted call caches nothing, and only a completed final retry is cached. | `cache.rs`: `interrupted_call_cannot_cache_empty_outcome_for_retry`, `retry_cache_requires_explicit_completed_final_attempt` |

## Proof search

`install` builds the lean-semantic-search runtime into `<store>/semantic-runtimes` (`project/semantic_runtime.rs`); a
server only opens it, and a runtime that needs a build fails the call with the install command. Opening runs
`lake build --no-build LeanSemanticSearch` in the runtime's package once per toolchain and sysroot in each server
process, at the first search that needs it, and again after a failed open. It builds nothing. It takes no
`max_lean_runs` permit and has no child file, but it holds `children.lock`, and `[build] timeout_secs` limits its wait
for the cache entry's lock and then its run. Each search runs the runtime's driver, `LeanSemanticSearchDriver.lean`, as
a short run (`broker/semantic_file.rs`), and the contract crate's `DriverReply::parse` checks the reply before the
server reads a row. `tools/proof_search.rs` merges those hits with lean-rs declaration search and ranks them.

| Invariant | Test |
| --- | --- |
| A reply from another contract version fails the search, and each hit takes its module, name and kind from its row. | `semantic_search.rs`: `a_reply_from_another_contract_fails_the_search`, `candidates_take_module_name_and_kind_from_their_row` |
| A runtime that needs a build tells the operator to run `install` again. | `project/semantic_preparation_tests.rs`: `an_unbuilt_runtime_tells_the_operator_to_rerun_install` |

## Transport, replies and config

Stdio and Streamable HTTP build the same `LeanHostService` over one `ProjectBroker`, so HTTP sessions share the broker's
worker caps, queues and lifecycle state. `transport_http.rs` owns the axum and rmcp wiring; HTTP types never reach the
tools, the broker or the project runtime. Stdio ends on the client's EOF. `--bind` selects HTTP, which binds only a
loopback address, rejects every request that carries an `Origin` with 403, and checks the `Host` header. The SDK serves
stateless discovery for MCP 2026-07-28 and the initialize handshake for older clients.

The semantic tools turn internal `Response<T>` envelopes (`envelope.rs`) into `{ data, errors, trust }`. Runtime
pressure stays in `errors`; transport and protocol failures use HTTP or MCP errors. `trust` (`trust.rs`) carries the
facts a client needs before it relies on a reply, such as each artifact's status and the source digest. Quiet telemetry
drops timing but keeps trust facts and warnings. The service boundary picks the output carrier. `request_log.rs` logs a
summary of each call without its content.

`config_file.rs` reads the two [config files](operations.md#configuration) at the Lake root and no others, and
`config_schema.rs` holds every setting in one table, which generates the
[reference](operations.md#configuration-reference).

| Invariant | Test |
| --- | --- |
| HTTP binds only loopback and rejects browser origins and foreign hosts; five HTTP clients share one broker. | `main.rs`: `non_loopback_bind_is_rejected`, `http_path_without_bind_is_rejected`; `tests/http_transport.rs`: `native_http_rejects_every_browser_origin_and_foreign_host`, `streamable_http_five_clients_check_the_same_source_bytes` |
| The server exits on stdin EOF or SIGTERM, and its worker exits with it. | `tests/stdio_lifecycle.rs`: `stdio_shutdown_on_stdin_eof_exits_parent`, `stdio_shutdown_after_tool_call_exits_worker_child`; `tests/http_transport.rs`: `streamable_http_sigterm_shutdown_exits` |
| `lean_status` reports the resolved settings without opening a worker, and `lean_verify` reports progress. | `tests/http_transport.rs`: `streamable_http_status_reports_resolved_settings_without_opening_worker`; `tests/stdio_lifecycle.rs`: `stdio_lean_verify_reports_progress` |
| A semantic error keeps its domain data in every carrier, and quiet telemetry keeps trust facts and warnings. | `server.rs`: `semantic_error_flag_preserves_domain_data_and_all_carriers`; `tools/semantic.rs`: `quiet_telemetry_does_not_drop_trust_artifacts`; `envelope.rs`: `recovered_retry_warning_survives_quiet_telemetry` |
| A check reply names the digest of the text it read, and only when it read one text. | `tools/semantic.rs`: `a_check_reply_names_the_digest_of_the_text_it_read`; `trust.rs`: `a_reply_names_a_source_digest_only_for_one_text` |
| Build rows name `lean_build` with module targets, and trust rows keep the order they first came in. | `trust.rs`: `build_rows_name_lean_build_with_module_targets`, `trust_deduplication_preserves_first_occurrence_order` |
| The slow-call warning keys on CPU time and stays in quiet replies. | `tools/semantic.rs`: `slow_call_warning_keys_on_cpu_and_rides_quiet_responses` |
| The request log holds no content and bounds its metadata. | `request_log.rs`: `summaries_pair_ids_preserve_negative_verdict_counts_and_omit_content`, `metadata_is_bounded_and_does_not_echo_unknown_kinds_or_paths` |
| Request synonyms map to one canonical shape, and canonical fields win. | `tools/request_aliases.rs`: `canonical_fields_win_over_synonyms`, `declaration_accepts_declaration_for_name_and_maps_field_synonyms`, `kind_synonyms_and_cross_tool_hints` |
| The tool catalog documents each semantic tool. | `tools/semantic.rs`: `tool_catalog_documents_semantic_surface` |
| The server reads config only at the Lake root, the local file wins per key, a file that does not parse stops startup and is named, and an empty variable counts as unset. | `config_file.rs`: `only_the_lake_root_is_read`, `local_file_overrides_the_tracked_file_per_key`, `a_file_that_does_not_parse_is_an_error_naming_it`, `a_setting_treats_an_empty_env_var_as_unset` |
| The generated reference and the defaults match the code. | `config_schema.rs`: `operations_md_reference_table_is_in_sync`, `generated_defaults_match_builtin_constants` |
| `install` writes only the local file and refuses to rewrite a broken one. | `cli/install.rs`: `install_records_its_paths_in_the_local_file_only`, `install_refuses_to_rewrite_a_broken_local_file` |
