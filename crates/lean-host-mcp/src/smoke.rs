//! Post-build runtime smoke test for an installed worker binary.
//!
//! `install-worker` proves a worker *built* and records the `lean.h` digest it
//! built against, but a matching digest does **not** imply the worker can
//! actually run: a toolchain's `libleanshared` can be ABI-incompatible with
//! this lean-rs build and crash (SIGSEGV) the moment real Lean code loads
//! through the FFI boundary. The header digest is a necessary but insufficient
//! signal; the only sound one is *did the worker run Lean*.
//!
//! [`probe`] runs the cheapest faithful exercise of that path: open a session
//! importing `Init` and inspect `Nat.add_zero` — the exact minimal operation
//! observed to crash incompatible workers. A shims-only open with no imports is
//! not enough; an incompatible worker survives that and only dies once it loads
//! real oleans through the ABI.
//!
//! The cost (spawn + `Init` import + a trivial inspect, ~1–2s) is paid once, at
//! install, against a multi-minute build. Recording the verdict in the
//! provenance sidecar lets every later project-open trust cheap recorded state
//! instead of re-probing on the hot path — complexity pulled down into install
//! rather than up into every call.

use std::path::Path;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerDeclarationInspectionRequest, LeanWorkerError, LeanWorkerHostHandleBuilder,
    LeanWorkerProcessTracker,
};
use serde::{Deserialize, Serialize};

use crate::toolchain::ToolchainId;

/// Whether a freshly-built worker survived a trivial real elaboration.
///
/// Recorded in the `worker.json` provenance sidecar so the readiness gate can
/// demote a worker that builds and digest-matches yet cannot run. The tag is
/// `"result"` so a sidecar reads `"smoke": { "result": "passed" }`.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum SmokeOutcome {
    /// The worker loaded Lean and completed the probe (the inspect result
    /// itself — found, not-found, or unsupported — is irrelevant; surviving is
    /// the proof).
    Passed,
    /// The worker failed the probe: a crash, a handshake/bootstrap failure, or
    /// any inability to complete the trivial inspect. `detail` is the platform
    /// exit status (e.g. `"signal: 11 (SIGSEGV)"`) or the error message.
    Failed { detail: String },
}

impl SmokeOutcome {
    /// One-word label for the `install-worker --list` `runtime` column:
    /// `runs` (the worker ran Lean) or `crashed` (it could not).
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Passed => "runs",
            Self::Failed { .. } => "crashed",
        }
    }

    /// The crash/error detail when the worker failed, else `None`.
    pub(crate) fn failure_detail(&self) -> Option<&str> {
        match self {
            Self::Passed => None,
            Self::Failed { detail } => Some(detail),
        }
    }
}

/// A cold worker importing `Init` can take a second or two; this runs once at
/// install, so the budget is generous rather than tight.
const SMOKE_STARTUP_TIMEOUT: Duration = Duration::from_mins(1);

/// Spawn `worker_path` against `lean_sysroot`, load Lean, and run a trivial real
/// elaboration. Returns [`SmokeOutcome::Failed`] for any crash, bootstrap
/// failure, or inability to complete the probe — it never panics and never
/// returns an `Err`, so the caller always gets a recordable verdict.
///
/// `toolchain` selects the compiled host compatibility expectation; resolution
/// of the worker binary and sysroot is the caller's job (it just built them).
pub(crate) fn probe(worker_path: &Path, lean_sysroot: &Path, toolchain: &ToolchainId) -> SmokeOutcome {
    let writer = match crate::shared_writer::contract() {
        Ok(writer) => writer,
        Err(error) => {
            return SmokeOutcome::Failed {
                detail: format!("smoke helper configuration failed: {error}"),
            };
        }
    };
    probe_with_writer(
        worker_path,
        lean_sysroot,
        toolchain,
        writer,
        &LeanWorkerProcessTracker::new(),
    )
}

fn probe_with_writer(
    worker_path: &Path,
    lean_sysroot: &Path,
    toolchain: &ToolchainId,
    writer: crate::shared_writer::WriterContract,
    tracker: &LeanWorkerProcessTracker,
) -> SmokeOutcome {
    // A throwaway working directory: the probe imports only `Init` (resident in
    // the toolchain's sysroot), so `LeanHost::from_lake_project` needs the root
    // to be a directory, not a real Lake project.
    let project_root = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return SmokeOutcome::Failed {
                detail: format!("could not create smoke working directory for {toolchain}: {e}"),
            };
        }
    };

    // macOS temporary directories commonly use /var, an alias of /private/var.
    // The child reports canonical custody domains, and the supervisor compares
    // that receipt with the configured project root before permitting a body.
    let canonical_root = match project_root.path().canonicalize() {
        Ok(root) => root,
        Err(error) => {
            return SmokeOutcome::Failed {
                detail: format!("could not canonicalize smoke working directory for {toolchain}: {error}"),
            };
        }
    };
    let started = Instant::now();
    let remaining = || SMOKE_STARTUP_TIMEOUT.saturating_sub(started.elapsed());
    // Preparation never enters Lean or the native generation. The install caller
    // must drain the common registry before a genuine writer is needed.
    let prepared =
        match crate::host_preparation::probe_ready(worker_path, lean_sysroot, &writer.executable, remaining(), &|| {
            false
        }) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => match crate::host_preparation::prepare_cancellable(
                worker_path,
                lean_sysroot,
                &writer.executable,
                remaining(),
                &|| false,
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    return SmokeOutcome::Failed {
                        detail: format!("smoke artifact preparation failed: {error}"),
                    };
                }
            },
            Err(error) => {
                return SmokeOutcome::Failed {
                    detail: format!("smoke artifact readiness failed: {error}"),
                };
            }
        };
    if let Err(error) = std::fs::create_dir(canonical_root.join(".lake")) {
        return SmokeOutcome::Failed {
            detail: format!("could not prepare private smoke workspace: {error}"),
        };
    }
    let expectation = match crate::runtime_compatibility::session_expectation(toolchain.as_str(), None) {
        Ok(expectation) => expectation,
        Err(detail) => return SmokeOutcome::Failed { detail },
    };
    // An install smoke owns a temporary unscoped generation; it must never
    // invent a broker capacity limit or share an unrelated actor's slot.
    let child = LeanWorkerChild::for_toolchain(worker_path, lean_sysroot)
        .shared_writer_executable(writer.executable.clone())
        .native_guardian(writer.executable)
        .lifetime_directory(writer.ownership_registry)
        .prepared_host_artifacts(prepared)
        .shared_artifact_reads(true);
    let open = LeanWorkerHostHandleBuilder::shims_only(&canonical_root, std::iter::empty::<String>())
        .require_host_runtime(expectation)
        .worker_child(child)
        .process_tracker(tracker.clone())
        .startup_timeout(remaining())
        .request_timeout(SMOKE_STARTUP_TIMEOUT)
        .open();
    let mut handle = match open {
        Ok(handle) => handle,
        Err(error) => {
            let mut detail = describe(&error);
            if tracker.active_processes() != 0 {
                let retained = project_root.keep();
                detail = format!(
                    "{detail}; native scope closure remains unconfirmed; retained smoke workspace {}",
                    retained.display()
                );
            }
            return SmokeOutcome::Failed { detail };
        }
    };

    // The exact minimal operation observed to crash ABI-incompatible workers:
    // open a session that loads `Init` through the FFI boundary, then inspect a
    // core declaration. Any `Ok` — including `NotFound`/`Unsupported` — means
    // the worker survived; only an infrastructure `Err` is a smoke failure.
    let request = LeanWorkerDeclarationInspectionRequest::new("Nat.add_zero");
    let outcome = match handle.inspect_declaration_with_imports(vec!["Init".to_owned()], &request, None, None) {
        Ok(_) => SmokeOutcome::Passed,
        Err(err) => SmokeOutcome::Failed { detail: describe(&err) },
    };

    // Install readiness requires the actual owned native scope to close. A
    // returned inspection result never substitutes for a reaped generation.
    if let Err(error) = handle.try_shutdown() {
        let retained = project_root.keep();
        return SmokeOutcome::Failed {
            detail: format!(
                "smoke cleanup unconfirmed: {}; retained workspace {}",
                describe(&error),
                retained.display()
            ),
        };
    }
    outcome
}

/// Render a worker error into a short detail string, preferring the child's
/// platform exit status (which names the signal, e.g. `signal: 11 (SIGSEGV)`)
/// when the failure was a process death.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "only the two process-death variants carry an exit status to prefer; LeanWorkerError is upstream-evolving and every other variant falls back to its Display string"
)]
fn describe(err: &LeanWorkerError) -> String {
    match err {
        LeanWorkerError::ChildPanicOrAbort { exit } | LeanWorkerError::ChildExited { exit } => exit.status.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn non_worker_fails_before_native_startup_without_unbounded_wait() {
        let toolchain = ToolchainId::parse("v4.35.0").expect("supported toolchain");
        let started = Instant::now();
        let result = probe(
            Path::new("/usr/bin/true"),
            Path::new("/nonexistent/sysroot"),
            &toolchain,
        );
        assert!(matches!(result, SmokeOutcome::Failed { detail } if detail.contains("artifact readiness failed")));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Runs the same guarded install probe with an actual matching worker and
    /// helper. The common registry must be drained, just as for installation.
    #[test]
    #[ignore = "requires LEAN_HOST_MCP_SMOKE_WORKER, LEAN_HOST_MCP_SMOKE_HELPER and a drained native registry"]
    fn actual_guarded_smoke_uses_canonical_temporary_project_custody() {
        let worker = std::env::var_os("LEAN_HOST_MCP_SMOKE_WORKER").expect("actual worker path");
        let helper = std::env::var_os("LEAN_HOST_MCP_SMOKE_HELPER").expect("actual helper path");
        let toolchain = ToolchainId::parse("v4.35.0").expect("supported toolchain");
        let sysroot = toolchain.elan_dir().expect("installed test toolchain");
        let mut writer = crate::shared_writer::contract().expect("common native namespace");
        writer.executable = std::path::PathBuf::from(helper)
            .canonicalize()
            .expect("canonical helper");
        let tracker = LeanWorkerProcessTracker::new();
        let outcome = probe_with_writer(Path::new(&worker), &sysroot, &toolchain, writer, &tracker);
        assert_eq!(
            outcome,
            SmokeOutcome::Passed,
            "actual guarded Init/Nat.add_zero smoke must complete and reap"
        );
        assert_eq!(
            tracker.active_processes(),
            0,
            "all actual native scopes must be physically closed"
        );
    }

    /// The writer controller is another test process so its registry binding is
    /// private without mutating this multithreaded test process's environment.
    #[test]
    #[ignore = "internal actual warm-handoff writer controller"]
    fn actual_warm_handoff_writer_child() {
        use std::process::Command;
        let helper = std::env::var_os("LEAN_HOST_MCP_SMOKE_HELPER").expect("actual helper");
        let root = std::path::PathBuf::from(
            std::env::var_os("LEAN_HOST_MCP_WARM_TEST_ROOT").expect("private warm-handoff root"),
        );
        let mut writer =
            lean_toolchain::shared_writer::SharedWriter::open(Path::new(&helper), &root, Duration::from_secs(15))
                .expect("writer demand eventually receives actual exclusive custody");
        let mut command = Command::new("/usr/bin/touch");
        command.arg(root.join("writer-finished"));
        let result = writer
            .run(&command, Duration::from_secs(5))
            .expect("one actual writer stage");
        assert!(result.status.success(), "the actual writer stage completed normally");
    }

    /// Bypass broker admission to reproduce the race after a caller has already
    /// borrowed its warm actor. The guardian still owns the old SH descriptions.
    #[test]
    #[ignore = "requires matching worker/helper and explicit canonical LEAN_RS_WORKER_OWNERS_DIRECTORY"]
    fn actual_warm_worker_contention_cycles_before_one_target_body() {
        use std::fs;
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
        use std::process::{Child, Command, Stdio};

        use lean_rs_worker_parent::{
            LeanWorkerElabOptions, LeanWorkerModuleQuery, LeanWorkerModuleQueryOutcome, LeanWorkerModuleQueryResult,
        };
        use lean_toolchain::writer_demand::ReadAdmission;

        struct WriterController(Child);
        impl Drop for WriterController {
            fn drop(&mut self) {
                if matches!(self.0.try_wait(), Ok(None)) {
                    // Only this test's actual unreaped controller. Its EOF
                    // leaves real H responsible for any native cleanup.
                    let _kill_result = self.0.kill();
                    let _wait_result = self.0.wait();
                }
            }
        }

        let worker = std::path::PathBuf::from(std::env::var_os("LEAN_HOST_MCP_SMOKE_WORKER").expect("worker"))
            .canonicalize()
            .expect("canonical worker");
        let helper = std::path::PathBuf::from(std::env::var_os("LEAN_HOST_MCP_SMOKE_HELPER").expect("helper"))
            .canonicalize()
            .expect("canonical helper");
        let toolchain = ToolchainId::parse("v4.35.0").expect("supported toolchain");
        let sysroot = toolchain.elan_dir().expect("installed matching toolchain");
        // Keep the private guardian socket below Darwin's pathname limit.
        let project = tempfile::Builder::new()
            .prefix("warm-")
            .tempdir_in("/tmp")
            .expect("private warm-handoff workspace");
        let workspace = project.path().canonicalize().expect("canonical private workspace");
        let root = workspace.join("project");
        fs::create_dir(&root).expect("private consumer root");
        fs::create_dir(root.join(".lake")).expect("private Lake domain");
        fs::write(root.join("lake-manifest.json"), "{\"packages\":[]}\n").expect("fixed empty dependency graph");
        // The parent validates this explicit lifetime directory against the
        // inherited registry used by readiness helpers and the writer client.
        // The opt-in test caller creates and binds a private registry first.
        let registry = std::path::PathBuf::from(
            std::env::var_os(lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV)
                .expect("explicit private native registry for the actual warm test"),
        );
        assert!(
            registry.is_absolute() && registry.is_dir(),
            "existing absolute private registry required"
        );
        assert_eq!(registry.canonicalize().expect("canonical private registry"), registry);
        let prepared =
            crate::host_preparation::probe_ready(&worker, &sysroot, &helper, SMOKE_STARTUP_TIMEOUT, &|| false)
                .expect("actual runtime-free readiness")
                .expect("actual bundled artifacts must already be ready");
        let expectation = crate::runtime_compatibility::session_expectation(toolchain.as_str(), None)
            .expect("compiled matching ABI expectation");
        let tracker = LeanWorkerProcessTracker::new();
        let child = LeanWorkerChild::for_toolchain(&worker, &sysroot)
            .shared_writer_executable(helper.clone())
            .native_guardian(helper.clone())
            .lifetime_directory(&registry)
            .prepared_host_artifacts(prepared)
            .shared_artifact_reads(true);
        let mut handle = LeanWorkerHostHandleBuilder::shims_only(&root, std::iter::empty::<String>())
            .require_host_runtime(expectation)
            .worker_child(child)
            .process_tracker(tracker.clone())
            .startup_timeout(SMOKE_STARTUP_TIMEOUT)
            .request_timeout(Duration::from_secs(15))
            .open()
            .expect("actual private guarded worker");
        let mut writer = None;
        let result = catch_unwind(AssertUnwindSafe(|| {
            handle
                .inspect_declaration_with_imports(
                    vec!["Init".to_owned()],
                    &LeanWorkerDeclarationInspectionRequest::new("Nat.add_zero"),
                    None,
                    None,
                )
                .expect("warm actual Init session");
            let old_metadata = handle.host_runtime_metadata().expect("actual warm metadata").clone();
            let before = handle.lifecycle_snapshot();
            handle
                .release_host_read()
                .expect("close logical request aliases, retain generation custody");
            assert_eq!(
                tracker.active_processes(),
                1,
                "the warm generation still owns its guardian scope"
            );
            let counter = root.join("target-effects");
            fs::write(&counter, "").expect("private target counter");
            writer = Some(WriterController(
                Command::new(std::env::current_exe().expect("test executable"))
                    .args(["--ignored", "--exact", "smoke::tests::actual_warm_handoff_writer_child"])
                    .env("LEAN_HOST_MCP_SMOKE_HELPER", &helper)
                    .env("LEAN_HOST_MCP_WARM_TEST_ROOT", &root)
                    .env(lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV, &registry)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .expect("actual private writer controller"),
            ));
            let lock = lean_toolchain::shared_writer::project_lock_path(&root).expect("exact project domain");
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                match ReadAdmission::try_acquire(std::slice::from_ref(&lock)) {
                    Ok(None) => break,
                    Ok(Some(_)) => {}
                    Err(error) => assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock,
                        "handoff observation failed: {error}"
                    ),
                }
                assert!(Instant::now() < until, "actual writer did not announce demand");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                !root.join("writer-finished").exists(),
                "old guardian SH still excludes the writer"
            );
            let marker = serde_json::to_string(&root.join("writer-finished")).expect("Lean writer marker path");
            let counter_path = serde_json::to_string(&counter).expect("Lean target counter path");
            let source = format!(
                "#eval do\n  let before ← IO.FS.readFile {counter_path}\n  IO.FS.writeFile {counter_path} (before ++ \"body\\n\")\n  let _writer ← IO.FS.readFile {marker}\n  pure ()\n"
            );
            // Record every body entry before checking writer ordering. An
            // illicit early failure followed by a body retry would count twice.
            // One target submission. Only pre-body session admission may cycle.
            let outcome = handle
                .process_module_query_with_imports(
                    vec!["Init".to_owned()],
                    &source,
                    &LeanWorkerModuleQuery::Diagnostics,
                    &LeanWorkerElabOptions::default(),
                    None,
                    None,
                )
                .expect("one target runs after the contending writer and confirmed old-generation stop");
            assert!(
                matches!(
                    &outcome,
                    LeanWorkerModuleQueryOutcome::Ok {
                        result: LeanWorkerModuleQueryResult::Diagnostics(diagnostics), ..
                    } if !diagnostics.truncated && diagnostics.diagnostics.iter().all(|item| item.severity != "error")
                ),
                "actual target diagnostics must be clean: {outcome:?}"
            );
            assert_eq!(
                fs::read_to_string(&counter).expect("actual target effect"),
                "body\n",
                "body ran exactly once"
            );
            let after = handle.lifecycle_snapshot();
            assert_eq!(
                after.worker_generation,
                before.worker_generation.saturating_add(1),
                "one pre-body generation cycle"
            );
            assert_eq!(
                after.restarts,
                before.restarts.saturating_add(1),
                "no extra or body retry cycle"
            );
            let actual = handle.host_runtime_metadata().expect("actual replacement metadata");
            let old_epoch = old_metadata
                .artifact_read_epochs
                .as_ref()
                .expect("old actual read custody")
                .iter()
                .find(|epoch| Path::new(&epoch.project_root) == root)
                .expect("old private domain");
            let new_epoch = actual
                .artifact_read_epochs
                .as_ref()
                .expect("replacement actual read custody")
                .iter()
                .find(|epoch| Path::new(&epoch.project_root) == root)
                .expect("replacement private domain");
            assert_ne!(
                new_epoch.epoch, old_epoch.epoch,
                "actual writer epoch changed before replacement body"
            );
            assert_eq!(
                actual.checked_host_bindings, old_metadata.checked_host_bindings,
                "actual ABI remains checked"
            );
        }));
        // Preserve the fixture on any failure; a known proof result never
        // substitutes for confirmed native cleanup in this actual-image test.
        let shutdown = handle.try_shutdown();
        let writer_status = writer.as_mut().map(|controller| {
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                match controller.0.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                if Instant::now() >= until {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        if result.is_err()
            || shutdown.is_err()
            || tracker.active_processes() != 0
            || !matches!(writer_status, Some(Some(status)) if status.success())
        {
            let retained = project.keep();
            eprintln!("retained actual warm-handoff fixture: {}", retained.display());
        }
        if let Err(panic) = result {
            resume_unwind(panic);
        }
        shutdown.expect("confirmed replacement native scope closure");
        assert_eq!(
            tracker.active_processes(),
            0,
            "all actual generations physically closed"
        );
        assert!(
            matches!(writer_status, Some(Some(status)) if status.success()),
            "actual writer controller completed normally"
        );
    }

    #[test]
    fn outcome_round_trips_through_sidecar_json() {
        let passed = SmokeOutcome::Passed;
        let failed = SmokeOutcome::Failed {
            detail: "signal: 11 (SIGSEGV)".to_owned(),
        };
        for outcome in [&passed, &failed] {
            let json = serde_json::to_string(outcome).unwrap();
            let back: SmokeOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, outcome);
        }
        // The tag the gate and `--list` read.
        assert!(
            serde_json::to_string(&passed)
                .unwrap()
                .contains("\"result\":\"passed\"")
        );
        assert_eq!(passed.label(), "runs");
        assert_eq!(failed.label(), "crashed");
        assert_eq!(failed.failure_detail(), Some("signal: 11 (SIGSEGV)"));
        assert_eq!(passed.failure_detail(), None);
    }

    #[test]
    fn probe_of_a_non_worker_binary_fails_rather_than_panics() {
        // A binary that is not a worker cannot complete the handshake; the probe
        // must turn that into a recordable `Failed`, never a panic or `Err`.
        let toolchain = ToolchainId::parse("v4.30.0").unwrap();
        let bogus = Path::new("/bin/echo");
        let sysroot = std::env::temp_dir();
        assert!(matches!(
            probe(bogus, &sysroot, &toolchain),
            SmokeOutcome::Failed { .. }
        ));
    }
}
