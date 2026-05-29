//! The uniform response envelope every tool returns.
//!
//! Encoding contract:
//!
//! ```jsonc
//! {
//!   "status": "ok",
//!   "result":   { /* tool-specific; null for runtime_unavailable */ },
//!   "runtime_error": null,
//!   "freshness": {
//!     "project_root":   "/abs/path",
//!     "project_hash":   "sha256-hex",
//!     "imports":        ["Mod.A", "..."],
//!     "session_id":     "uuid",
//!     "lean_toolchain": "leanprover/lean4:v4.x.y"
//!   },
//!   "runtime": {
//!     "worker_generation": 1,
//!     "worker_restarted": false,
//!     "retry_count": 0,
//!     "admission_wait_millis": 0,
//!     "queue_wait_millis": 0,
//!     "call_restart": null,
//!     "last_restart": null
//!   },
//!   "warnings":     ["..."],     // omitted when empty
//!   "next_actions": ["..."]      // omitted when empty
//! }
//! ```
//!
//! `project_hash` is the Lake-manifest SHA-256. Clients can branch on
//! `(project_root, project_hash)` to detect dependency changes between
//! tool calls without a separate declaration search first.
//!
//! Three volatile decisions hide behind one shape: what freshness means,
//! how it's serialized, and what an MCP "warning" looks like. Tools don't
//! pick the layout; they build a `Response<T>` and let rmcp serialize it.

use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Freshness {
    pub project_root: String,
    pub project_hash: String,
    pub imports: Vec<String>,
    pub session_id: String,
    pub lean_toolchain: String,
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
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeFacts>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
}

impl<T> Response<T>
where
    T: Serialize + JsonSchema,
{
    pub fn ok(result: T, freshness: Freshness) -> Self {
        Self {
            status: ResponseStatus::Ok,
            result: Some(result),
            runtime_error: None,
            freshness,
            runtime: None,
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
    }

    pub fn runtime_unavailable(failure: RuntimeFailure, freshness: Freshness, runtime: RuntimeFacts) -> Self {
        Self {
            status: ResponseStatus::RuntimeUnavailable,
            result: None,
            runtime_error: Some(failure),
            freshness,
            runtime: Some(runtime),
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
    }

    pub fn result_ref(&self) -> Option<&T> {
        self.result.as_ref()
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: RuntimeFacts) -> Self {
        self.runtime = Some(runtime);
        self
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
