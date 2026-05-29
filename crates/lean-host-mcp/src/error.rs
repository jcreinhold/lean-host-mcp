//! The one error type tool handlers return.
//!
//! Most "failures" Lean reports back (parse error, elaboration error, missing
//! declaration) are *not* `ServerError`; they live in the tool's `result`
//! payload as structured data. `ServerError` is reserved for things the
//! caller cannot meaningfully recover from locally: the Lean runtime failed
//! to init, the project actor is busy or unavailable, or the Lake project
//! does not exist.

use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;

use crate::envelope::{Freshness, RuntimeFacts, RuntimeFailure};
use lean_rs_worker_parent::LeanWorkerError;

#[derive(Debug, Clone, Serialize, JsonSchema, thiserror::Error)]
#[error("worker unavailable: {reason}")]
pub struct WorkerUnavailable {
    pub retryable: bool,
    pub worker_restarted: bool,
    pub project_root: String,
    pub project_hash: String,
    pub imports: Vec<String>,
    pub session_id: String,
    pub lean_toolchain: String,
    pub worker_generation: u64,
    pub reason: String,
    pub restart_cause: Option<String>,
    pub rss_kib: Option<u64>,
    pub limit_kib: Option<u64>,
    pub retry_after_millis: Option<u64>,
    pub restarts_in_window: Option<u64>,
    pub window_millis: Option<u64>,
    pub runtime: RuntimeFacts,
}

impl WorkerUnavailable {
    pub(crate) fn freshness(&self) -> Freshness {
        Freshness {
            project_root: self.project_root.clone(),
            project_hash: self.project_hash.clone(),
            imports: self.imports.clone(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.lean_toolchain.clone(),
        }
    }

    pub(crate) fn failure(&self) -> RuntimeFailure {
        RuntimeFailure {
            reason: self.reason.clone(),
            retryable: self.retryable,
            project_root: self.project_root.clone(),
            session_id: self.session_id.clone(),
            worker_generation: self.worker_generation,
            worker_restarted: self.worker_restarted,
            restart_cause: self.restart_cause.clone(),
            rss_kib: self.rss_kib,
            limit_kib: self.limit_kib,
            retry_after_millis: self.retry_after_millis,
            restarts_in_window: self.restarts_in_window,
            window_millis: self.window_millis,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("lean runtime: {0}")]
    Lean(String),

    #[error("session thread is gone")]
    SessionGone,

    #[error("{0}")]
    WorkerUnavailable(Box<WorkerUnavailable>),

    #[error("lake project not usable: {0}")]
    BadProject(String),

    #[error("declaration index: {0}")]
    Index(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, ServerError>;

impl ServerError {
    pub(crate) fn worker_unavailable(info: WorkerUnavailable) -> Self {
        Self::WorkerUnavailable(Box::new(info))
    }
}

impl From<ServerError> for McpError {
    fn from(err: ServerError) -> Self {
        // -32603 == internal error in JSON-RPC. The MCP spec leaves wider
        // codes available but most clients only branch on this band.
        match err {
            ServerError::WorkerUnavailable(info) => {
                let message = info.to_string();
                let data = json!({
                    "retryable": info.retryable,
                    "worker_restarted": info.worker_restarted,
                    "project_root": &info.project_root,
                    "project_hash": &info.project_hash,
                    "imports": &info.imports,
                    "session_id": &info.session_id,
                    "lean_toolchain": &info.lean_toolchain,
                    "worker_generation": info.worker_generation,
                    "reason": &info.reason,
                    "restart_cause": &info.restart_cause,
                    "rss_kib": info.rss_kib,
                    "limit_kib": info.limit_kib,
                    "retry_after_millis": info.retry_after_millis,
                    "restarts_in_window": info.restarts_in_window,
                    "window_millis": info.window_millis,
                });
                Self::internal_error(message, Some(data))
            }
            other @ (ServerError::Lean(_)
            | ServerError::SessionGone
            | ServerError::BadProject(_)
            | ServerError::Index(_)
            | ServerError::Io(_)
            | ServerError::Internal(_)) => Self::internal_error(other.to_string(), None),
        }
    }
}

impl From<anyhow::Error> for ServerError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())
    }
}

/// Classify a worker-layer infrastructure error at the runtime boundary.
///
/// Bootstrap failures map to `ServerError::BadProject`; worker death and
/// timeout cases that need retry/restart metadata are handled by the project
/// actor before this fallback is used.
#[allow(
    clippy::needless_pass_by_value,
    clippy::wildcard_enum_match_arm,
    reason = "LeanWorkerError is upstream-evolving; everything outside the bootstrap-classification set maps to Lean for the MCP wire"
)]
pub(crate) fn map_worker_err(err: LeanWorkerError) -> ServerError {
    match err {
        LeanWorkerError::WorkerChildUnresolved { .. }
        | LeanWorkerError::WorkerChildNotExecutable { .. }
        | LeanWorkerError::Bootstrap { .. }
        | LeanWorkerError::CapabilityBuild { .. }
        | LeanWorkerError::Setup { .. }
        | LeanWorkerError::Handshake { .. }
        | LeanWorkerError::CapabilityMetadataMismatch { .. } => ServerError::BadProject(err.to_string()),
        LeanWorkerError::ChildPanicOrAbort { .. } | LeanWorkerError::ChildExited { .. } => ServerError::Lean(format!(
            "worker process exited; project worker will restart before the next request: {err}"
        )),
        _ => ServerError::Lean(err.to_string()),
    }
}
