//! rmcp server glue. Registers model-facing Lean tools and wires them to the
//! [`tools`](crate::tools) module.
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
    #[tool(description = "Inspect one Lean declaration by name or cursor.")]
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

    #[tool(description = "Bounded source/text search over Lean files.")]
    async fn source_search(
        &self,
        Parameters(req): Parameters<tools::scan::SourceSearchRequest>,
    ) -> std::result::Result<Json<Response<tools::scan::SourceSearchResult>>, McpError> {
        wrap(tools::scan::source_search(&self.ctx, req).await)
    }

    #[tool(description = "Proof context at a cursor.")]
    async fn proof_state(
        &self,
        Parameters(req): Parameters<tools::position::ProofStateRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ProofStateResult>>, McpError> {
        wrap(tools::position::proof_state(&self.ctx, req).await)
    }

    #[tool(description = "Run a bounded semantic query batch against one file.")]
    async fn lean_query(
        &self,
        Parameters(req): Parameters<tools::position::LeanQueryRequest>,
    ) -> std::result::Result<Json<Response<tools::position::LeanQueryResult>>, McpError> {
        wrap(tools::position::lean_query(&self.ctx, req).await)
    }

    #[tool(description = "Find references to a fully-qualified Lean name.")]
    async fn find_references(
        &self,
        Parameters(req): Parameters<tools::position::FindReferencesRequest>,
    ) -> std::result::Result<Json<Response<tools::position::FindReferencesResult>>, McpError> {
        wrap(tools::position::find_references(&self.ctx, req).await)
    }

    #[tool(description = "Advise Mathlib-compatible declaration placement.")]
    async fn mathlib_placement(
        &self,
        Parameters(req): Parameters<tools::placement::MathlibPlacementRequest>,
    ) -> std::result::Result<Json<Response<tools::placement::MathlibPlacementResult>>, McpError> {
        wrap(tools::placement::mathlib_placement(&self.ctx, req).await)
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
             attempts and verification, source search, reference lookup, \
             Mathlib placement advice, and expert semantic file queries."
                .to_owned(),
        );
        info
    }
}

fn wrap<T>(result: crate::error::Result<Response<T>>) -> std::result::Result<Json<Response<T>>, McpError>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    result.map(Json).map_err(McpError::from)
}
