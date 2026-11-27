//! The verdict of the runtime smoke test that `install` runs on a worker it
//! has just built (`cli/installed_shims.rs` `probe`).
//!
//! A worker that builds against the toolchain's `lean.h` can still crash once
//! real Lean code loads through the FFI boundary, so install opens a session
//! importing `Init` and inspects `Nat.add_zero`. It records the verdict in the
//! generation's `worker.json`, and every later open reads that record instead
//! of probing again.

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the round trip directly")]
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
}
