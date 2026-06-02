//! The uniform response envelope every tool returns.
//!
//! Encoding contract (default `quiet` telemetry verbosity):
//!
//! ```jsonc
//! {
//!   "status": "ok",
//!   "result":   { /* tool-specific; null for runtime_unavailable */ },
//!   "runtime_error": null,
//!   "freshness": {
//!     "project_root":   "/abs/path",
//!     "session_id":     "uuid",
//!     "lean_toolchain": "leanprover/lean4:v4.x.y"
//!   },
//!   "warnings":     ["..."],     // omitted when empty
//!   "next_actions": ["..."]      // omitted when empty
//! }
//! ```
//!
//! What the model reads is kept to proof-relevant content. Operational
//! telemetry is gated behind [`TelemetryVerbosity`](crate::tools::TelemetryVerbosity):
//! in the default `quiet` mode the `runtime` block is omitted unless it carries
//! an actionable signal (a worker restart — see [`RuntimeFacts::is_actionable`]),
//! and `freshness` drops `project_hash` (the Lake-manifest SHA-256) and the full
//! `imports` list. In `full` mode every field is emitted: `runtime` with its
//! lifecycle/pressure counters, and `freshness` with all five fields, so a
//! client can branch on `(project_root, project_hash)` to detect dependency
//! changes between calls.
//!
//! Three volatile decisions hide behind one shape: what freshness means,
//! how it's serialized, and what an MCP "warning" looks like. Tools don't
//! pick the layout; they build a `Response<T>` and `crate::server` serializes
//! it after [`Response::trim_telemetry`] applies the verbosity gate.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;

/// The project freshness snapshot a producer builds.
///
/// Built by [`crate::project`]'s `freshness` and
/// [`crate::error::WorkerUnavailable::freshness`]. Not serialized directly:
/// [`Response::ok`] splits it into the always-emitted [`FreshnessIdentity`] and
/// the verbosity-gated [`Telemetry`] block.
#[derive(Debug, Clone)]
pub struct Freshness {
    pub project_root: String,
    pub project_hash: String,
    pub imports: Vec<String>,
    pub session_id: String,
    pub lean_toolchain: String,
    /// Project-lifetime toolchain-provenance advisories (unknown pin, missing
    /// provenance sidecar). Carried to the envelope, where one drain
    /// ([`Response::drain_advisories`], called by [`crate::server`]) moves them
    /// into the top-level `warnings` array, so "warnings are top-level" holds.
    pub(crate) toolchain_advisories: Vec<String>,
}

/// Session identity — always serialized as the envelope's `freshness`.
///
/// Small, stable, and occasionally relevant to a proof agent: `session_id`
/// flips when the worker re-spawns, signalling a context reset.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FreshnessIdentity {
    pub project_root: String,
    pub session_id: String,
    pub lean_toolchain: String,
}

/// Operational telemetry, serialized as the envelope's `telemetry`.
///
/// Emitted only under `full`
/// [`TelemetryVerbosity`](crate::tools::TelemetryVerbosity). It carries
/// cache/identity metadata (`project_hash`, the full `imports` list) and the
/// worker `runtime` facts — none of which a proof agent needs to make progress,
/// so the default `quiet` mode drops the whole block. The one actionable signal
/// a restart carries already reaches the agent as a top-level `warning`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Telemetry {
    /// Lake-manifest SHA-256. Lets a client branch on `(project_root,
    /// project_hash)` to detect dependency changes between calls.
    pub project_hash: String,
    /// Caller-supplied import set for this session.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Worker lifecycle and admission-pressure facts for this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeFacts>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RuntimeRestartEvent {
    pub cause: String,
    pub reason: String,
    pub worker_generation: u64,
    pub planned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_kib: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RuntimeFacts {
    pub worker_generation: u64,
    pub worker_restarted: bool,
    pub retry_count: u32,
    pub admission_wait_millis: u64,
    pub queue_wait_millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_restart: Option<RuntimeRestartEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_restart: Option<RuntimeRestartEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_kib: Option<u64>,
    pub worker_lanes: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_profile: Option<String>,
    pub profile_switch_count: u64,
    /// Worker recycles observed over this project's lifetime, all causes. Lets
    /// a client see recycle *frequency* (the per-call cause is in `call_restart`).
    pub restarts_total: u64,
    /// Lifetime recycle count keyed by stable cause string (e.g. `rss_post_job`).
    /// Omitted when no recycle has happened.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub restarts_by_cause: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Ok,
    RuntimeUnavailable,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeFailure {
    pub reason: String,
    pub retryable: bool,
    pub project_root: String,
    pub session_id: String,
    pub worker_generation: u64,
    pub worker_restarted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_kib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_millis: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restarts_in_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_millis: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Response<T>
where
    T: Serialize + JsonSchema,
{
    pub status: ResponseStatus,
    pub result: Option<T>,
    pub runtime_error: Option<RuntimeFailure>,
    pub freshness: FreshnessIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<Telemetry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
    /// Project-lifetime advisories awaiting the drain into `warnings`. Never
    /// serialized; emptied by [`Response::drain_advisories`] at the boundary.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) advisories: Vec<String>,
}

/// Split a producer's [`Freshness`] snapshot into the serialized identity, the
/// telemetry block, and the advisory list the envelope drains into `warnings`.
fn split_freshness(freshness: Freshness, runtime: Option<RuntimeFacts>) -> (FreshnessIdentity, Telemetry, Vec<String>) {
    let Freshness {
        project_root,
        project_hash,
        imports,
        session_id,
        lean_toolchain,
        toolchain_advisories,
    } = freshness;
    (
        FreshnessIdentity {
            project_root,
            session_id,
            lean_toolchain,
        },
        Telemetry {
            project_hash,
            imports,
            runtime,
        },
        toolchain_advisories,
    )
}

impl<T> Response<T>
where
    T: Serialize + JsonSchema,
{
    pub fn ok(result: T, freshness: Freshness) -> Self {
        let (freshness, telemetry, advisories) = split_freshness(freshness, None);
        Self {
            status: ResponseStatus::Ok,
            result: Some(result),
            runtime_error: None,
            freshness,
            telemetry: Some(telemetry),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            advisories,
        }
    }

    pub fn runtime_unavailable(failure: RuntimeFailure, freshness: Freshness, runtime: RuntimeFacts) -> Self {
        let (freshness, telemetry, advisories) = split_freshness(freshness, Some(runtime));
        Self {
            status: ResponseStatus::RuntimeUnavailable,
            result: None,
            runtime_error: Some(failure),
            freshness,
            telemetry: Some(telemetry),
            warnings: Vec::new(),
            next_actions: Vec::new(),
            advisories,
        }
    }

    pub fn result_ref(&self) -> Option<&T> {
        self.result.as_ref()
    }

    /// The worker runtime facts, read from the telemetry block. `None` once the
    /// boundary gate has dropped telemetry (quiet verbosity) or before any
    /// runtime was attached.
    pub fn runtime(&self) -> Option<&RuntimeFacts> {
        self.telemetry.as_ref().and_then(|telemetry| telemetry.runtime.as_ref())
    }

    /// The caller-supplied import set, read from the telemetry block. Used by
    /// internal tool composition (e.g. `search_for_proof` reusing a
    /// `proof_state` response) before the boundary gate runs.
    pub fn imports(&self) -> &[String] {
        match &self.telemetry {
            Some(telemetry) => &telemetry.imports,
            None => &[],
        }
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: RuntimeFacts) -> Self {
        if let Some(telemetry) = self.telemetry.as_mut() {
            telemetry.runtime = Some(runtime);
        }
        self
    }

    /// Drop the telemetry block — the default `quiet` verbosity gate, applied at
    /// the serialization boundary. Identity, `result`, `warnings`,
    /// `next_actions`, and `runtime_error` are untouched, so no correctness or
    /// truncation signal is lost.
    pub fn drop_telemetry(&mut self) {
        self.telemetry = None;
    }

    /// Drain project-lifetime advisories into `warnings`. Called once by
    /// [`crate::server`] just before serialization, for both `ok` and
    /// `runtime_unavailable` responses.
    pub(crate) fn drain_advisories(&mut self) {
        let advisories = std::mem::take(&mut self.advisories);
        self.warnings.extend(advisories);
    }

    #[must_use]
    pub fn warn(mut self, msg: impl Into<String>) -> Self {
        self.warnings.push(msg.into());
        self
    }

    #[must_use]
    pub fn hint(mut self, msg: impl Into<String>) -> Self {
        self.next_actions.push(msg.into());
        self
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn runtime_facts_separate_call_restart_from_lifecycle_history() {
        let facts = RuntimeFacts {
            worker_generation: 4,
            worker_restarted: false,
            retry_count: 0,
            admission_wait_millis: 0,
            queue_wait_millis: 0,
            call_restart: None,
            last_restart: Some(RuntimeRestartEvent {
                cause: "rss_post_job".to_owned(),
                reason: "rss_post_job current_kib=5 limit_kib=4".to_owned(),
                worker_generation: 3,
                planned: true,
                rss_kib: Some(5),
                limit_kib: Some(4),
            }),
            rss_kib: Some(2),
            worker_lanes: 1,
            import_profile: Some("Init".to_owned()),
            profile_switch_count: 1,
            restarts_total: 1,
            restarts_by_cause: BTreeMap::from([("rss_post_job".to_owned(), 1)]),
        };

        let json = serde_json::to_value(facts).unwrap();
        assert!(json.pointer("/call_restart").is_none_or(serde_json::Value::is_null));
        assert_eq!(
            json.pointer("/last_restart/cause").and_then(serde_json::Value::as_str),
            Some("rss_post_job")
        );
        assert_eq!(
            json.pointer("/worker_restarted").and_then(serde_json::Value::as_bool),
            Some(false)
        );
    }
}
