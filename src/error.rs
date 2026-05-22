//! The one error type tool handlers return.
//!
//! Most "failures" Lean reports back (parse error, elaboration error, missing
//! declaration) are *not* `ServerError` — they live in the tool's `result`
//! payload as structured data. `ServerError` is reserved for things the
//! caller cannot meaningfully recover from: the Lean runtime failed to init,
//! the Lake project does not exist, the session thread panicked.

use rmcp::ErrorData as McpError;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("lean runtime: {0}")]
    Lean(String),

    #[error("session thread is gone")]
    SessionGone,

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
        McpError::internal_error(err.to_string(), None)
    }
}

impl From<anyhow::Error> for ServerError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(err.to_string())
    }
}
