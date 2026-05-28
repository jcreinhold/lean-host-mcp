//! The uniform response envelope every tool returns.
//!
//! Encoding contract:
//!
//! ```jsonc
//! {
//!   "result":   { /* tool-specific */ },
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
//!     "restart_reason": null
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
pub struct RuntimeFacts {
    pub worker_generation: u64,
    pub worker_restarted: bool,
    pub retry_count: u32,
    pub admission_wait_millis: u64,
    pub queue_wait_millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Response<T>
where
    T: Serialize + JsonSchema,
{
    pub result: T,
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
            result,
            freshness,
            runtime: None,
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
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
