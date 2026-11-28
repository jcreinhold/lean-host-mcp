//! Why a worker child was cycled: the causes this server names itself, the
//! event for a restart the supervisor made, the per-cause tally, and the log
//! line each recycle emits.

use std::collections::BTreeMap;
use std::time::Duration;

use lean_rs_worker_parent::LeanWorkerRestartReason;

use crate::envelope::RuntimeRestartEvent;

/// A restart cause this server names: a cycle it asked for, or the failure
/// that ended a call. The supervisor names its own restarts; see
/// [`worker_restart_event`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RestartCause {
    /// A `.olean` among the live session's imports was rebuilt on disk. The
    /// session's environment is a snapshot taken at import, so it must be
    /// dropped or the call answers from a stale Lean environment.
    ArtifactsRebuilt,
    /// Imports crossed the soft share of the residue budget while the mailbox
    /// was empty, so the cycle ran between requests instead of inside one. The
    /// ratio of this cause to the supervisor's `import_residue` is how much of
    /// the recycle cost the idle path moved off the request path.
    ImportResidueIdle,
    RssHardLimit,
    Timeout,
    Cancelled,
    ChildExit,
    ChildAbort,
    SessionMissing,
    /// A restart the supervisor reported without a reason, or several in one
    /// call, so that no one reason describes them.
    WorkerInternal,
}

impl RestartCause {
    /// The cause's name. A failure the supervisor also restarts for has the
    /// supervisor's name, so one tally key counts both.
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactsRebuilt => "artifacts_rebuilt",
            Self::ImportResidueIdle => "import_residue_idle",
            Self::RssHardLimit => "rss_hard_limit",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ChildExit => "child_exit",
            Self::ChildAbort => "child_abort",
            Self::SessionMissing => "session_missing",
            Self::WorkerInternal => "worker_internal",
        }
    }

    /// Whether this server asked for the restart. Its two cycles replace a
    /// working child between requests; every other cause ended a call.
    pub(super) const fn is_planned(self) -> bool {
        matches!(self, Self::ArtifactsRebuilt | Self::ImportResidueIdle)
    }
}

pub(super) fn restart_event(
    cause: RestartCause,
    reason: impl Into<String>,
    worker_generation: u64,
    rss_kib: Option<u64>,
    limit_kib: Option<u64>,
) -> RuntimeRestartEvent {
    RuntimeRestartEvent {
        cause: cause.as_str().to_owned(),
        reason: reason.into(),
        worker_generation,
        planned: cause.is_planned(),
        rss_kib,
        limit_kib,
    }
}

/// The event for a restart the supervisor made, in the supervisor's terms:
/// its cause name, whether it was planned, and its one-line description,
/// which for a child that died ends with the child's last stderr.
pub(super) fn worker_restart_event(
    reason: &LeanWorkerRestartReason,
    worker_generation: u64,
    rss_kib: Option<u64>,
) -> RuntimeRestartEvent {
    RuntimeRestartEvent {
        cause: reason.stable_cause().to_owned(),
        reason: reason.to_string(),
        worker_generation,
        planned: reason.is_planned(),
        rss_kib,
        limit_kib: None,
    }
}

/// Per-project recycle tally over the worker's lifetime, all causes.
///
/// Recorded once per event at [`super::ProjectActorState::record_restart`] and copied
/// into [`super::RuntimeSnapshot`] on publish, so the no-call and error paths report
/// the same totals a live call would. This answers "how *often*, and why?"; the
/// single most-recent event stays in `last_restart`.
#[derive(Debug, Clone, Default)]
pub(super) struct RestartStats {
    pub(super) total: u64,
    pub(super) by_cause: BTreeMap<String, u64>,
}

impl RestartStats {
    pub(super) fn observe(&mut self, cause: &str) {
        self.total = self.total.saturating_add(1);
        let count = self.by_cause.entry(cause.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

/// Emit one structured log line for a recycle: `warn` for an abnormal
/// restart, `info` for a planned one an operator tunes a budget by, and
/// `debug` for the other planned ones.
pub(super) fn log_restart(event: &RuntimeRestartEvent, restarts_total: u64) {
    macro_rules! emit {
        ($level:ident, $msg:literal) => {
            tracing::$level!(
                cause = %event.cause,
                reason = %event.reason,
                worker_generation = event.worker_generation,
                rss_kib = ?event.rss_kib,
                limit_kib = ?event.limit_kib,
                planned = event.planned,
                restarts_total,
                $msg
            )
        };
    }
    if !event.planned {
        emit!(warn, "worker recycled (abnormal)");
        return;
    }
    match event.cause.as_str() {
        "rss_ceiling" => emit!(info, "worker recycled (memory pressure)"),
        "artifacts_rebuilt" => emit!(info, "worker recycled (imports rebuilt on disk)"),
        // Reactive at `info`, proactive at `debug` (via the fallthrough): the
        // ratio between them is the tuning signal, and it is the *reactive*
        // half — the cycle a call had to wait for — that an operator acts on.
        "import_residue" => emit!(info, "worker recycled (import residue budget)"),
        "import_accounting_unknown" => emit!(
            info,
            "worker retired after an import error; retained memory could not be measured"
        ),
        _ => emit!(debug, "worker recycled (hygiene)"),
    }
}

pub(super) fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "unit tests use expect/unwrap_err to state the branch under test directly"
)]
mod tests {
    use super::*;

    #[test]
    fn restart_stats_tally_total_and_per_cause() {
        let mut stats = RestartStats::default();
        stats.observe("import_residue");
        stats.observe("import_residue");
        stats.observe("child_abort");

        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_cause.get("import_residue"), Some(&2));
        assert_eq!(stats.by_cause.get("child_abort"), Some(&1));
        assert_eq!(stats.by_cause.get("idle"), None);
    }

    #[test]
    fn only_the_servers_own_cycles_are_planned() {
        assert!(RestartCause::ArtifactsRebuilt.is_planned());
        assert!(RestartCause::ImportResidueIdle.is_planned());
        for cause in [
            RestartCause::RssHardLimit,
            RestartCause::Timeout,
            RestartCause::Cancelled,
            RestartCause::ChildExit,
            RestartCause::ChildAbort,
            RestartCause::SessionMissing,
            RestartCause::WorkerInternal,
        ] {
            assert!(!cause.is_planned(), "{}", cause.as_str());
        }
    }

    /// A call that fails for a reason the supervisor also restarts for must
    /// use the supervisor's name, or the tally would count one cause under
    /// two keys.
    #[test]
    fn a_failure_the_supervisor_restarts_for_has_the_supervisors_name() {
        let pairs = [
            (
                RestartCause::RssHardLimit,
                LeanWorkerRestartReason::RssHardLimit {
                    operation: "test",
                    current_kib: 2,
                    limit_kib: 1,
                    last_import_stats: None,
                },
            ),
            (
                RestartCause::Timeout,
                LeanWorkerRestartReason::RequestTimeout {
                    operation: "test",
                    duration: Duration::from_millis(1),
                },
            ),
            (
                RestartCause::Cancelled,
                LeanWorkerRestartReason::Cancelled { operation: "test" },
            ),
            (
                RestartCause::ChildExit,
                LeanWorkerRestartReason::ChildExit {
                    operation: "test",
                    status: "exit status: 0".to_owned(),
                    diagnostics: String::new(),
                },
            ),
            (
                RestartCause::ChildAbort,
                LeanWorkerRestartReason::ChildAbort {
                    operation: "test",
                    status: "signal: 6 (SIGABRT)".to_owned(),
                    diagnostics: String::new(),
                },
            ),
            (
                RestartCause::SessionMissing,
                LeanWorkerRestartReason::SessionMissing { operation: "test" },
            ),
        ];
        for (cause, reason) in pairs {
            assert_eq!(cause.as_str(), reason.stable_cause());
            assert_eq!(cause.is_planned(), reason.is_planned(), "{}", cause.as_str());
        }
    }

    /// The supervisor's description is the only place the figures an operator
    /// needs, such as what was retained and what the limit was, or what the
    /// child printed as it died, reach the response.
    #[test]
    fn a_supervisor_restart_keeps_its_name_class_and_description() {
        let residue = worker_restart_event(
            &LeanWorkerRestartReason::ImportResidue {
                residue_bytes: 9 * 1024 * 1024 * 1024,
                limit_bytes: 12 * 1024 * 1024 * 1024,
            },
            3,
            Some(7),
        );
        assert_eq!(residue.cause, "import_residue");
        assert_eq!(residue.reason, "import_residue residue_mib=9216 limit_mib=12288");
        assert!(residue.planned);
        assert_eq!((residue.worker_generation, residue.rss_kib), (3, Some(7)));

        let abort = worker_restart_event(
            &LeanWorkerRestartReason::ChildAbort {
                operation: "worker_attempt_proof",
                status: "signal: 6 (SIGABRT)".to_owned(),
                diagnostics: "out of memory\n".to_owned(),
            },
            4,
            None,
        );
        assert_eq!(abort.cause, "child_abort");
        assert_eq!(
            abort.reason,
            "child_abort operation=worker_attempt_proof status=signal: 6 (SIGABRT): out of memory"
        );
        assert!(!abort.planned);

        let start = worker_restart_event(
            &LeanWorkerRestartReason::StartFailed {
                error: "no such file".to_owned(),
            },
            5,
            None,
        );
        assert_eq!(start.cause, "start_failed");
        assert!(!start.planned);
    }
}
