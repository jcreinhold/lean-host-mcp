//! Why a worker child was cycled: the cause names the parent reports, which
//! causes count toward the crash-loop breaker, the per-cause tally, and the
//! log line each recycle emits.

use std::collections::BTreeMap;
use std::time::Duration;

use lean_rs_worker_parent::{LeanWorkerError, LeanWorkerRestartReason};

use crate::envelope::RuntimeRestartEvent;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RestartCause {
    /// A `.olean` among the live session's imports was rebuilt on disk. The
    /// session's environment is a snapshot taken at import, so it must be
    /// dropped or the call answers from a stale Lean environment.
    ArtifactsRebuilt,
    RssPostJob,
    RssHardLimit,
    MaxRequests,
    MaxImports,
    /// Imports retained the configured budget of unreclaimable bytes, and the
    /// child was cycled before the next one on the request path.
    ImportResidue,
    /// A failed header import left retained memory unmeasured. Retire the child
    /// before another request; the failed request keeps its original diagnostics.
    ImportAccountingUnknown,
    /// The same budget crossed its soft threshold while the mailbox was empty,
    /// so the cycle ran between requests instead of inside one. The ratio of
    /// this cause to [`Self::ImportResidue`] is how much of the recycle cost the
    /// idle path actually moved off the critical path.
    ImportResidueIdle,
    Idle,
    Timeout,
    Cancelled,
    ChildExit,
    ChildAbort,
    SessionMissing,
    Explicit,
    WorkerInternal,
}

impl RestartCause {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactsRebuilt => "artifacts_rebuilt",
            Self::RssPostJob => "rss_post_job",
            Self::RssHardLimit => "rss_hard_limit_exceeded",
            Self::MaxRequests => "max_requests",
            Self::MaxImports => "max_imports",
            Self::ImportResidue => "import_residue",
            Self::ImportAccountingUnknown => "import_accounting_unknown",
            Self::ImportResidueIdle => "import_residue_idle",
            Self::Idle => "idle",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ChildExit => "child_exit",
            Self::ChildAbort => "child_abort",
            Self::SessionMissing => "session_missing",
            Self::Explicit => "explicit",
            Self::WorkerInternal => "worker_internal",
        }
    }

    pub(super) const fn counts_toward_restart_limit(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Cancelled
                | Self::ChildExit
                | Self::ChildAbort
                | Self::SessionMissing
                | Self::RssHardLimit
                // Not a planned cycle: this is the bucket for a restart whose
                // cause this build cannot name — an upstream policy added after
                // it shipped, or a replacement the supervisor reported with no
                // reason at all. `diagnosis::execution_taint` already treats it
                // as disrupting; a crash-loop breaker that excused the one cause
                // it understands least would be exactly backwards.
                | Self::WorkerInternal
        )
    }

    pub(super) const fn is_planned(self) -> bool {
        !self.counts_toward_restart_limit()
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

/// Emit one structured log line for a recycle. Level tracks the *signal*, not
/// `planned`: crash/abnormal causes `warn`, memory-pressure cycles `info` (the
/// frequency an operator tuning the RSS budget watches), pure hygiene `debug`.
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
    match event.cause.as_str() {
        "rss_hard_limit_exceeded"
        | "child_abort"
        | "child_exit"
        | "session_missing"
        | "worker_internal"
        | "timeout"
        | "cancelled" => emit!(warn, "worker recycled (abnormal)"),
        "rss_post_job" => emit!(info, "worker recycled (memory pressure)"),
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

pub(super) fn restart_cause_from_worker(reason: &LeanWorkerRestartReason) -> RestartCause {
    restart_cause_from_worker_name(reason.stable_cause())
}

fn restart_cause_from_worker_name(cause: &str) -> RestartCause {
    match cause {
        "explicit" => RestartCause::Explicit,
        "max_requests" => RestartCause::MaxRequests,
        "max_imports" => RestartCause::MaxImports,
        "import_residue" => RestartCause::ImportResidue,
        "import_accounting_unknown" => RestartCause::ImportAccountingUnknown,
        "rss_ceiling" => RestartCause::RssPostJob,
        "rss_hard_limit" => RestartCause::RssHardLimit,
        "idle" => RestartCause::Idle,
        "cancelled" => RestartCause::Cancelled,
        "timeout" => RestartCause::Timeout,
        "child_abort" => RestartCause::ChildAbort,
        _ => RestartCause::WorkerInternal,
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "only worker process death variants are restart causes; all other errors are classified elsewhere"
)]
pub(super) fn worker_death_cause(err: &LeanWorkerError) -> RestartCause {
    match err {
        LeanWorkerError::ChildPanicOrAbort { .. } => RestartCause::ChildAbort,
        LeanWorkerError::ChildExited { .. } => RestartCause::ChildExit,
        _ => RestartCause::WorkerInternal,
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "protocol 13 accounting retirement is handled by stable name above the match, preserving compatibility with published lean-rs"
)]
pub(super) fn restart_reason_text(reason: &LeanWorkerRestartReason) -> String {
    // Match the stable name to retain compatibility with the published lean-rs
    // version while protocol 13 is prepared for release.
    if reason.stable_cause() == RestartCause::ImportAccountingUnknown.as_str() {
        return "import_accounting_unknown: retained memory could not be measured after an import error".to_owned();
    }
    match reason {
        LeanWorkerRestartReason::Explicit => RestartCause::Explicit.as_str().to_owned(),
        LeanWorkerRestartReason::MaxRequests { limit } => format!("max_requests limit={limit}"),
        LeanWorkerRestartReason::MaxImports { limit } => format!("max_imports limit={limit}"),
        LeanWorkerRestartReason::ImportResidue {
            residue_bytes,
            limit_bytes,
        } => format!(
            "import_residue residue_mib={} limit_mib={}",
            residue_bytes / (1024 * 1024),
            limit_bytes / (1024 * 1024)
        ),
        LeanWorkerRestartReason::RssCeiling {
            current_kib, limit_kib, ..
        } => {
            format!("rss_ceiling current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::RssHardLimit {
            operation,
            current_kib,
            limit_kib,
            ..
        } => {
            format!("rss_hard_limit operation={operation} current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::Idle { idle_for, limit } => {
            format!(
                "idle idle_for_millis={} limit_millis={}",
                millis_u64(*idle_for),
                millis_u64(*limit)
            )
        }
        LeanWorkerRestartReason::Cancelled { operation } => {
            format!("cancelled operation={operation}")
        }
        // The child's stderr is drained once, at exit, and retained nowhere
        // else. Dropping it here would leave `child_abort operation=…` as the
        // whole record of a fatal child — enough to know something died, not
        // enough to tell a Lean out-of-memory panic from a segfault.
        LeanWorkerRestartReason::ChildAbort {
            operation,
            status,
            diagnostics,
        } => {
            let detail = diagnostics.trim();
            if detail.is_empty() {
                format!("child_abort operation={operation} status={status}")
            } else {
                format!("child_abort operation={operation} status={status}: {detail}")
            }
        }
        LeanWorkerRestartReason::RequestTimeout { operation, duration } => {
            format!(
                "timeout operation={operation} duration_millis={}",
                millis_u64(*duration)
            )
        }
        // `LeanWorkerRestartReason` is `#[non_exhaustive]`: a cycling policy
        // added upstream must surface as an unnamed cause rather than fail to
        // build here. `restart_cause_from_worker` maps the same case to
        // `WorkerInternal`, so the two agree.
        other => format!("worker_internal reason={}", other.stable_cause()),
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
        stats.observe("rss_post_job");
        stats.observe("rss_post_job");
        stats.observe("child_abort");

        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_cause.get("rss_post_job"), Some(&2));
        assert_eq!(stats.by_cause.get("child_abort"), Some(&1));
        assert_eq!(stats.by_cause.get("idle"), None);
    }

    #[test]
    fn planned_restart_causes_do_not_consume_abnormal_restart_budget() {
        assert!(!RestartCause::RssPostJob.counts_toward_restart_limit());
        assert!(!RestartCause::MaxRequests.counts_toward_restart_limit());
        assert!(!RestartCause::MaxImports.counts_toward_restart_limit());
        assert!(!RestartCause::ImportResidue.counts_toward_restart_limit());
        assert!(!RestartCause::ImportAccountingUnknown.counts_toward_restart_limit());
        assert!(!RestartCause::ImportResidueIdle.counts_toward_restart_limit());
        assert!(!RestartCause::Idle.counts_toward_restart_limit());

        assert!(RestartCause::ChildExit.counts_toward_restart_limit());
        assert!(RestartCause::ChildAbort.counts_toward_restart_limit());
        assert!(RestartCause::Timeout.counts_toward_restart_limit());
        assert!(RestartCause::Cancelled.counts_toward_restart_limit());
        assert!(RestartCause::SessionMissing.counts_toward_restart_limit());
        assert!(RestartCause::RssHardLimit.counts_toward_restart_limit());
    }

    #[test]
    fn unknown_import_accounting_is_a_planned_retirement_not_a_crash() {
        let cause = restart_cause_from_worker_name("import_accounting_unknown");
        assert_eq!(cause, RestartCause::ImportAccountingUnknown);
        assert_eq!(cause.as_str(), "import_accounting_unknown");
        assert!(cause.is_planned());
        assert!(!cause.counts_toward_restart_limit());
        assert!(restart_cause_from_worker_name("unrecognized_policy").counts_toward_restart_limit());
    }

    #[test]
    fn worker_restart_reason_maps_to_stable_cause() {
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::MaxRequests { limit: 1 }).as_str(),
            "max_requests"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssCeiling {
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_post_job"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssHardLimit {
                operation: "test",
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_hard_limit_exceeded"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RequestTimeout {
                operation: "test",
                duration: Duration::from_millis(1),
            })
            .as_str(),
            "timeout"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::ImportResidue {
                residue_bytes: 2,
                limit_bytes: 1,
            })
            .as_str(),
            "import_residue"
        );
    }

    /// The reason string is the only place the two figures an operator needs —
    /// what was retained and what the limit was — reach the response, which is
    /// why they are not duplicated onto `RuntimeRestartEvent`.
    #[test]
    fn the_residue_reason_text_names_both_figures() {
        let text = restart_reason_text(&LeanWorkerRestartReason::ImportResidue {
            residue_bytes: 9 * 1024 * 1024 * 1024,
            limit_bytes: 12 * 1024 * 1024 * 1024,
        });
        assert_eq!(text, "import_residue residue_mib=9216 limit_mib=12288");
    }

    /// `LeanWorkerRestartReason` is `#[non_exhaustive]` and its `stable_cause`
    /// set is open, so a lean-rs upgrade can hand this build a cause it has
    /// never heard of. It lands on `worker_internal`, which must be treated as
    /// abnormal: `diagnosis::execution_taint` already counts it as disrupting,
    /// and a crash-loop breaker that excused the one cause it understands least
    /// would be exactly backwards. Every cause this build *does* know is
    /// classified precisely, so nothing routine reaches that bucket.
    #[test]
    fn an_unclassified_restart_is_abnormal_and_nothing_routine_reaches_it() {
        assert!(RestartCause::WorkerInternal.counts_toward_restart_limit());
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::ChildAbort {
                operation: "test",
                status: String::from("signal: 6 (SIGABRT)"),
                diagnostics: String::new(),
            })
            .as_str(),
            "child_abort",
            "a child abort is a named cause, not an unclassified one"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::Explicit).as_str(),
            "explicit"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::Idle {
                idle_for: Duration::from_millis(2),
                limit: Duration::from_millis(1),
            })
            .as_str(),
            "idle"
        );
    }
}
