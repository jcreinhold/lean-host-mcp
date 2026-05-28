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

#[derive(Debug, Clone, Serialize, JsonSchema, thiserror::Error)]
#[error("worker unavailable: {reason}")]
pub struct WorkerUnavailable {
    pub retryable: bool,
    pub worker_restarted: bool,
    pub project_root: String,
    pub session_id: String,
    pub worker_generation: u64,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("lean runtime: {0}")]
    Lean(String),

    #[error("session thread is gone")]
    SessionGone,

    #[error(transparent)]
    WorkerUnavailable(WorkerUnavailable),

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

impl From<ServerError> for McpError {
    fn from(err: ServerError) -> Self {
        // -32603 == internal error in JSON-RPC. The MCP spec leaves wider
        // codes available but most clients only branch on this band.
        match err {
            ServerError::WorkerUnavailable(info) => {
                let data = json!({
                    "retryable": info.retryable,
                    "worker_restarted": info.worker_restarted,
                    "project_root": info.project_root,
                    "session_id": info.session_id,
                    "worker_generation": info.worker_generation,
                    "reason": info.reason,
                });
                Self::internal_error(info.to_string(), Some(data))
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
