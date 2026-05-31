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
use std::time::Duration;

use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerDeclarationInspectionRequest, LeanWorkerError, LeanWorkerHostHandleBuilder,
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
/// `toolchain` is used only for diagnostics framing; resolution of the worker
/// binary and sysroot is the caller's job (it just built them).
pub(crate) fn probe(worker_path: &Path, lean_sysroot: &Path, toolchain: &ToolchainId) -> SmokeOutcome {
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

    let open = LeanWorkerHostHandleBuilder::shims_only(project_root.path(), std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(worker_path, lean_sysroot))
        .startup_timeout(SMOKE_STARTUP_TIMEOUT)
        .long_running_requests()
        .open();
    let mut handle = match open {
        Ok(handle) => handle,
        Err(err) => return SmokeOutcome::Failed { detail: describe(&err) },
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

    // Best-effort graceful shutdown; the verdict is already decided and a
    // terminate error on an already-dead child is not news.
    drop(handle.terminate());
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
