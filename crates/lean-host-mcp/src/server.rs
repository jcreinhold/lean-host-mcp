//! rmcp server glue. Registers model-facing Lean tools and wires them to the
//! [`tools`] module.
//!
//! Each `#[tool]` handler is a thin call into the implementation function;
//! all real work happens in `crate::tools` and `crate::project`. Returns
//! `Json<Response<T>>` so rmcp generates structured-content output and a
//! schema downstream clients can introspect.

use std::sync::Arc;

use rmcp::Json;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::broker::ProjectBroker;
use crate::envelope::Response;
use crate::error::ServerError;
use crate::tools::{self, ToolContext};

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
    pub fn new(broker: Arc<ProjectBroker>) -> Self {
        let ctx = ToolContext { broker };
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl LeanHostService {
    #[tool(description = "Inspect one Lean declaration by name.")]
    async fn inspect_declaration(
        &self,
        Parameters(req): Parameters<tools::declaration::InspectDeclarationRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::DeclarationInspectionResult>>, McpError> {
        wrap(tools::declaration::inspect_declaration(&self.ctx, req).await)
    }

    #[tool(description = "Return ranked declarations for the next proof step.")]
    async fn search_for_proof(
        &self,
        Parameters(req): Parameters<tools::proof_search::SearchForProofRequest>,
    ) -> std::result::Result<Json<Response<tools::proof_search::SearchForProofResult>>, McpError> {
        wrap(tools::proof_search::search_for_proof(&self.ctx, req).await)
    }

    #[tool(description = "Try proof snippets in memory. Never writes files.")]
    async fn try_proof_step(
        &self,
        Parameters(req): Parameters<tools::proof_action::TryProofStepRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::ProofAttemptResult>>, McpError> {
        wrap(tools::proof_action::try_proof_step(&self.ctx, req).await)
    }

    #[tool(description = "Verify one declaration in memory. Never writes files.")]
    async fn verify_declaration(
        &self,
        Parameters(req): Parameters<tools::proof_action::VerifyDeclarationRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::DeclarationVerificationResult>>, McpError> {
        wrap(tools::proof_action::verify_declaration(&self.ctx, req).await)
    }

    #[tool(description = "Proof context for a declaration proof position.")]
    async fn proof_state(
        &self,
        Parameters(req): Parameters<tools::position::ProofStateRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ProofStateResult>>, McpError> {
        wrap(tools::position::proof_state(&self.ctx, req).await)
    }

    #[tool(description = "Find references to a fully-qualified Lean name.")]
    async fn find_references(
        &self,
        Parameters(req): Parameters<tools::position::FindReferencesRequest>,
    ) -> std::result::Result<Json<Response<tools::position::FindReferencesResult>>, McpError> {
        wrap(tools::position::find_references(&self.ctx, req).await)
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
             Tools expose a bounded proof-agent workflow: proof context, \
             proof retrieval, declaration inspection, non-mutating proof \
             attempts and verification, and semantic reference lookup."
                .to_owned(),
        );
        info
    }
}

fn wrap<T>(result: crate::error::Result<Response<T>>) -> std::result::Result<Json<Response<T>>, McpError>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    match result {
        Ok(response) => Ok(finalize(response)),
        Err(ServerError::WorkerUnavailable(info)) => {
            let freshness = info.freshness();
            let failure = info.failure();
            Ok(finalize(Response::runtime_unavailable(
                failure,
                freshness,
                info.runtime.clone(),
            )))
        }
        Err(err) => Err(McpError::from(err)),
    }
}

/// Drain project-lifetime toolchain advisories (unknown pin, missing provenance
/// sidecar, no smoke record) carried on `freshness` into the top-level
/// `warnings` array.
///
/// This is the single funnel every tool response passes through, so the
/// contract "warnings are top-level" holds without each handler re-plumbing
/// them — and it applies identically to `ok` and `runtime_unavailable`
/// responses, so a worker that dies mid-call keeps its advisories instead of
/// silently dropping them.
fn finalize<T>(mut response: Response<T>) -> Json<Response<T>>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    let advisories = std::mem::take(&mut response.freshness.toolchain_advisories);
    response.warnings.extend(advisories);
    Json(response)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
        let response = wrap::<serde_json::Value>(Err(ServerError::worker_unavailable(WorkerUnavailable {
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
            // survive onto the failure envelope, not vanish when the worker dies.
            toolchain_advisories: vec!["worker for v4.30.0 has no runtime smoke record".to_owned()],
        })))
        .unwrap()
        .0;

        let json = serde_json::to_value(response).unwrap();
        assert_eq!(
            json.pointer("/status").and_then(serde_json::Value::as_str),
            Some("runtime_unavailable")
        );
        assert!(json.pointer("/result").is_none_or(serde_json::Value::is_null));
        assert_eq!(
            json.pointer("/runtime_error/retryable")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            json.pointer("/runtime_error/restart_cause")
                .and_then(serde_json::Value::as_str),
            Some("child_exit")
        );
        assert_eq!(
            json.pointer("/runtime/retry_count").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        // Finding #3: the open-time advisory is drained onto the failure
        // envelope's top-level warnings rather than dropped.
        assert_eq!(
            json.pointer("/warnings/0").and_then(serde_json::Value::as_str),
            Some("worker for v4.30.0 has no runtime smoke record")
        );
    }
}
