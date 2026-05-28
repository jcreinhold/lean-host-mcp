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
    #[tool(
        description = "Elaborate a Lean term against the project environment. Returns success or structured diagnostics."
    )]
    async fn elaborate(
        &self,
        Parameters(req): Parameters<tools::lean::ElaborateRequest>,
    ) -> std::result::Result<Json<Response<tools::lean::ElaborateResult>>, McpError> {
        wrap(tools::lean::elaborate(&self.ctx, req).await)
    }

    #[tool(
        description = "Kernel-check a Lean declaration source. Returns Checked / Rejected / Unavailable / Unsupported plus diagnostics."
    )]
    async fn kernel_check(
        &self,
        Parameters(req): Parameters<tools::lean::KernelCheckRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::KernelOutcome>>, McpError> {
        wrap(tools::lean::kernel_check(&self.ctx, req).await)
    }

    #[tool(description = "Infer the type of a Lean term via Meta.inferType.")]
    async fn infer_type(
        &self,
        Parameters(req): Parameters<tools::lean::InferTypeRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::MetaOutcome>>, McpError> {
        wrap(tools::lean::infer_type(&self.ctx, req).await)
    }

    #[tool(description = "Reduce a Lean term to weak-head normal form via Meta.whnf.")]
    async fn whnf(
        &self,
        Parameters(req): Parameters<tools::lean::WhnfRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::MetaOutcome>>, McpError> {
        wrap(tools::lean::whnf(&self.ctx, req).await)
    }

    #[tool(description = "Check whether two Lean terms are definitionally equal via Meta.isDefEq.")]
    async fn is_def_eq(
        &self,
        Parameters(req): Parameters<tools::lean::IsDefEqRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::MetaOutcome>>, McpError> {
        wrap(tools::lean::is_def_eq(&self.ctx, req).await)
    }

    #[tool(
        description = "Inspect one Lean declaration by name or cursor position, returning bounded statement, documentation, source, and metadata useful for proof work."
    )]
    async fn inspect_declaration(
        &self,
        Parameters(req): Parameters<tools::declaration::InspectDeclarationRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::DeclarationInspectionResult>>, McpError> {
        wrap(tools::declaration::inspect_declaration(&self.ctx, req).await)
    }

    #[tool(
        description = "Proof-agent retrieval: from a cursor or explicit goal/type text, return bounded ranked declarations likely to help the next proof step."
    )]
    async fn search_for_proof(
        &self,
        Parameters(req): Parameters<tools::proof_search::SearchForProofRequest>,
    ) -> std::result::Result<Json<Response<tools::proof_search::SearchForProofResult>>, McpError> {
        wrap(tools::proof_search::search_for_proof(&self.ctx, req).await)
    }

    #[tool(
        description = "Try one or more proof snippets against an in-memory Lean source overlay. Never writes files."
    )]
    async fn try_proof_step(
        &self,
        Parameters(req): Parameters<tools::proof_action::TryProofStepRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::ProofAttemptResult>>, McpError> {
        wrap(tools::proof_action::try_proof_step(&self.ctx, req).await)
    }

    #[tool(
        description = "Verify one Lean declaration in an in-memory source snapshot under a sorry/axiom policy. Never writes files."
    )]
    async fn verify_declaration(
        &self,
        Parameters(req): Parameters<tools::proof_action::VerifyDeclarationRequest>,
    ) -> std::result::Result<Json<Response<crate::projections::DeclarationVerificationResult>>, McpError> {
        wrap(tools::proof_action::verify_declaration(&self.ctx, req).await)
    }

    #[tool(
        description = "Filesystem regex sweep over the project's .lean files. Presets: sorry, admit, axiom, set_option."
    )]
    async fn project_scan(
        &self,
        Parameters(req): Parameters<tools::scan::ProjectScanRequest>,
    ) -> std::result::Result<Json<Response<tools::scan::ProjectScanResult>>, McpError> {
        wrap(tools::scan::project_scan(&self.ctx, req).await)
    }

    #[tool(
        description = "Default proof-agent context at a cursor: diagnostics, goals, locals, expected type, and safe edit spans."
    )]
    async fn proof_state(
        &self,
        Parameters(req): Parameters<tools::position::ProofStateRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ProofStateResult>>, McpError> {
        wrap(tools::position::proof_state(&self.ctx, req).await)
    }

    #[tool(description = "Expert tool: run a bounded custom batch of Lean semantic projections against one file.")]
    async fn lean_query(
        &self,
        Parameters(req): Parameters<tools::position::LeanQueryRequest>,
    ) -> std::result::Result<Json<Response<tools::position::LeanQueryResult>>, McpError> {
        wrap(tools::position::lean_query(&self.ctx, req).await)
    }

    #[tool(
        description = "Binder and use-site occurrences of a fully-qualified Lean name in one .lean file. Bounded and file-scoped."
    )]
    async fn references_in_file(
        &self,
        Parameters(req): Parameters<tools::position::ReferencesInFileRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ReferencesInFileResult>>, McpError> {
        wrap(tools::position::references_in_file(&self.ctx, req).await)
    }

    #[tool(
        description = "Explicit project-wide scan for binder and use-site occurrences of a fully-qualified Lean name. Optional files and limit bound the walk."
    )]
    async fn references_in_project(
        &self,
        Parameters(req): Parameters<tools::position::ReferencesInProjectRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ReferencesInProjectResult>>, McpError> {
        wrap(tools::position::references_in_project(&self.ctx, req).await)
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
             Tools elaborate / kernel-check terms, run bounded MetaM ops, \
             inspect one selected declaration, try and verify proof actions \
             without writing files, scan .lean files, and run bounded \
             proof-context and semantic file queries."
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
