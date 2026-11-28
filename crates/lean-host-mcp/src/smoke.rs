//! The verdict of the runtime smoke test that `install` runs on a worker it
//! has just built (`cli/installed_shims.rs` `probe`).
//!
//! A worker that builds against the toolchain's `lean.h` can still crash once
//! real Lean code loads through the FFI boundary, so install opens a session
//! importing `Init` and inspects `Nat.add_zero`. It records the verdict in the
//! generation's `worker.json`, and every later open reads that record instead
//! of probing again.

use serde::{Deserialize, Serialize};

/// Evidence that a freshly built worker survived a trivial real elaboration.
///
/// A failed probe fails the install, which then selects no generation, so a
/// record holds this verdict or none. The tag is `"result"`, so a record reads
/// `"smoke": { "result": "passed" }`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum SmokeOutcome {
    /// The worker loaded Lean, found `Nat.add_zero` and shut down.
    Passed,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests assert the round trip directly")]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trips_through_record_json() {
        let json = serde_json::to_string(&SmokeOutcome::Passed).unwrap();
        assert_eq!(json, r#"{"result":"passed"}"#);
        assert_eq!(
            serde_json::from_str::<SmokeOutcome>(&json).unwrap(),
            SmokeOutcome::Passed
        );
        // An older release recorded failed probes; this release reads none.
        assert!(serde_json::from_str::<SmokeOutcome>(r#"{"result":"failed","detail":"signal: 11"}"#).is_err());
    }
}
