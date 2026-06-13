//! rmcp server glue. Registers model-facing Lean tools and wires them to the
//! [`tools`] module.
//!
//! Each `#[tool]` handler is a thin call into the implementation function;
//! all real work happens in `crate::tools` and `crate::project`. Handlers
//! return a hand-built [`CallToolResult`] rather than `Json<T>`: the
//! `Json` wrapper makes rmcp advertise a deep `outputSchema` that no Anthropic
//! API client forwards to the model (it costs only wire bytes and breaks strict
//! schema validators), so we drop it and place the serialized envelope per the
//! configured [`ResponseCarrier`].

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::broker::ProjectBroker;
use crate::error::ServerError;
use crate::tools::{self, ResponseCarrier, ToolConfig, ToolContext};

// Deliberately not `use crate::error::Result;` here: the `#[tool_handler]`
// macro emits bare `Result<...>` references that must resolve to the std
// `Result`. `crate::error::Result` appears only via fully-qualified paths.

#[derive(Debug, Clone)]
pub struct LeanHostService {
    ctx: ToolContext,
    // Read by the `#[tool_handler]`-generated `call_tool` dispatcher; the
    // reference is invisible to dead-code analysis.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl LeanHostService {
    pub fn new(broker: Arc<ProjectBroker>, config: ToolConfig) -> Self {
        let ctx = ToolContext { broker, config };
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl LeanHostService {
    #[tool(
        description = "Lean proof or term context. Use kind=\"proof_position\" for proof state at a declaration position."
    )]
    async fn lean_context(
        &self,
        Parameters(req): Parameters<tools::semantic::SemanticToolRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tracing::debug!(tool = "lean_context", "tool call");
        self.respond_semantic(tools::semantic::lean_context(&self.ctx, req).await)
    }

    #[tool(description = "Non-mutating Lean experiments. Use kind=\"proof_step\" to try proof snippets in memory.")]
    async fn lean_trial(
        &self,
        Parameters(req): Parameters<tools::semantic::SemanticToolRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tracing::debug!(tool = "lean_trial", "tool call");
        self.respond_semantic(tools::semantic::lean_trial(&self.ctx, req).await)
    }

    #[tool(description = "Verify Lean declarations. Use kind=\"explicit\" for one named declaration in one file.")]
    async fn lean_verify(
        &self,
        Parameters(req): Parameters<tools::semantic::SemanticToolRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tracing::debug!(tool = "lean_verify", "tool call");
        self.respond_semantic(tools::semantic::lean_verify(&self.ctx, req).await)
    }

    #[tool(description = "Semantic lookup. Kinds: declaration, proof_search, references.")]
    async fn lean_lookup(
        &self,
        Parameters(req): Parameters<tools::semantic::SemanticToolRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tracing::debug!(tool = "lean_lookup", "tool call");
        self.respond_semantic(tools::semantic::lean_lookup(&self.ctx, req).await)
    }

    #[tool(description = "Cheap project, toolchain, and host status. Does not open a Lean worker.")]
    async fn lean_status(
        &self,
        Parameters(req): Parameters<tools::semantic::SemanticToolRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tracing::debug!(tool = "lean_status", "tool call");
        self.respond_semantic(tools::semantic::lean_status(&self.ctx, req))
    }
}

#[tool_handler]
impl ServerHandler for LeanHostService {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]` from rmcp; struct literal
        // syntax (even with `..default`) is forbidden across crates. Build
        // via Default + field mutation.
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_website_url("https://github.com/jcreinhold/lean-host-mcp");
        info.instructions = Some(
            "MCP server hosting Lean 4 in-process via lean-rs. \
             Tools expose five semantic Lean job families: context, trial, \
             verification, lookup, and status. Select a mode with each tool's \
             `kind` field."
                .to_owned(),
        );
        info
    }
}

impl LeanHostService {
    fn respond_semantic(
        &self,
        result: crate::error::Result<tools::semantic::SemanticResponse<serde_json::Value>>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let response = match result {
            Ok(response) => response,
            Err(ServerError::WorkerUnavailable(info)) => tools::semantic::from_worker_unavailable(&info),
            Err(err) => return Err(McpError::from(err)),
        };
        Ok(carry(&response, self.ctx.config.carrier))
    }
}

/// Serialize the response and place it in the tool result per the carrier:
/// `Text` emits one `content` text block (the model's read surface), `Both`
/// also mirrors it into `structuredContent` for code-mode clients, and
/// `Structured` emits only `structuredContent`.
fn carry<T>(response: &T, carrier: ResponseCarrier) -> CallToolResult
where
    T: serde::Serialize + schemars::JsonSchema,
{
    let value = match serde_json::to_value(response) {
        Ok(value) => value,
        Err(err) => {
            return CallToolResult::error(vec![Content::text(format!("failed to serialize response: {err}"))]);
        }
    };
    match carrier {
        ResponseCarrier::Text => CallToolResult::success(vec![Content::text(value.to_string())]),
        ResponseCarrier::Both => CallToolResult::structured(value),
        ResponseCarrier::Structured => {
            let mut result = CallToolResult::structured(value);
            result.content.clear();
            result
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::unreachable)]
mod tests {
    use super::*;
    use crate::envelope::{RuntimeFacts, RuntimeRestartEvent};
    use crate::error::{ServerError, WorkerUnavailable};

    #[test]
    fn worker_unavailable_is_a_structured_tool_response() {
        let runtime = RuntimeFacts {
            worker_generation: 7,
            retry_count: 1,
            call_restart: Some(RuntimeRestartEvent {
                cause: "child_exit".to_owned(),
                reason: "worker_death".to_owned(),
                worker_generation: 7,
                planned: false,
                rss_kib: Some(42),
                limit_kib: Some(100),
            }),
            worker_lanes: 1,
            ..RuntimeFacts::default()
        };
        let error = ServerError::worker_unavailable(WorkerUnavailable {
            retryable: true,
            worker_restarted: true,
            project_root: "/tmp/project".to_owned(),
            project_hash: "hash".to_owned(),
            imports: vec!["Init".to_owned()],
            session_id: "session".to_owned(),
            lean_toolchain: "leanprover/lean4:v4.30.0".to_owned(),
            worker_generation: 7,
            reason: "worker_death".to_owned(),
            restart_cause: Some("child_exit".to_owned()),
            rss_kib: Some(42),
            limit_kib: Some(100),
            retry_after_millis: None,
            restarts_in_window: Some(1),
            window_millis: Some(60_000),
            runtime,
            // A no-record / suspect worker carries an open-time advisory; it must
            // survive onto the semantic issue channel, not vanish when the worker dies.
            toolchain_advisories: vec!["worker for v4.30.0 has no runtime smoke record".to_owned()],
        });
        let ServerError::WorkerUnavailable(info) = error else {
            unreachable!("constructed a WorkerUnavailable error")
        };
        let response = tools::semantic::from_worker_unavailable(&info);

        let json = serde_json::to_value(&response).unwrap();
        assert!(json.pointer("/data").is_none_or(serde_json::Value::is_null));
        assert_eq!(
            json.pointer("/errors/0/code").and_then(serde_json::Value::as_str),
            Some("runtime_unavailable")
        );
        assert_eq!(
            json.pointer("/errors/0/retryable").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            json.pointer("/errors/0/details/restart_cause")
                .and_then(serde_json::Value::as_str),
            Some("child_exit")
        );
        assert_eq!(
            json.pointer("/trust/session_id").and_then(serde_json::Value::as_str),
            Some("session")
        );
        // The open-time advisory is preserved as a warning issue.
        assert_eq!(
            json.pointer("/errors/1/message").and_then(serde_json::Value::as_str),
            Some("worker for v4.30.0 has no runtime smoke record")
        );
    }
}
