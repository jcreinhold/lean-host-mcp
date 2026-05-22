//! rmcp server glue. Registers seven tools and wires them to the
//! [`tools`](crate::tools) module.
//!
//! Each `#[tool]` handler is a thin call into the implementation function;
//! all real work happens in `crate::tools` and `crate::session`. Returns
//! `Json<Response<T>>` so rmcp generates structured-content output and a
//! schema downstream clients can introspect.

use rmcp::Json;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::envelope::Response;
use crate::session::SessionHost;
use crate::tools::{self, ToolContext};

// Deliberately not `use crate::error::Result;` here — the `#[tool_handler]`
// macro emits bare `Result<...>` references that must resolve to the std
// `Result`. We use `crate::error::Result` only via fully-qualified paths.

#[derive(Debug, Clone)]
pub struct LeanHostService {
    ctx: ToolContext,
    // Read by the `#[tool_handler]`-generated `call_tool` dispatcher; the
    // reference is invisible to dead-code analysis.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl LeanHostService {
    pub fn new(host: SessionHost) -> Self {
        let ctx = ToolContext {
            lake_root: host.lake_root().to_owned(),
            default_imports: Vec::new(),
            host,
        };
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
    ) -> std::result::Result<Json<Response<crate::session::KernelOutcome>>, McpError> {
        wrap(tools::lean::kernel_check(&self.ctx, req).await)
    }

    #[tool(description = "Infer the type of a Lean term via Meta.inferType.")]
    async fn infer_type(
        &self,
        Parameters(req): Parameters<tools::lean::InferTypeRequest>,
    ) -> std::result::Result<Json<Response<crate::session::MetaOutcome>>, McpError> {
        wrap(tools::lean::infer_type(&self.ctx, req).await)
    }

    #[tool(description = "Reduce a Lean term to weak-head normal form via Meta.whnf.")]
    async fn whnf(
        &self,
        Parameters(req): Parameters<tools::lean::WhnfRequest>,
    ) -> std::result::Result<Json<Response<crate::session::MetaOutcome>>, McpError> {
        wrap(tools::lean::whnf(&self.ctx, req).await)
    }

    #[tool(description = "Check whether two Lean terms are definitionally equal via Meta.isDefEq.")]
    async fn is_def_eq(
        &self,
        Parameters(req): Parameters<tools::lean::IsDefEqRequest>,
    ) -> std::result::Result<Json<Response<crate::session::MetaOutcome>>, McpError> {
        wrap(tools::lean::is_def_eq(&self.ctx, req).await)
    }

    #[tool(description = "Look up a Lean declaration by fully-qualified name.")]
    async fn hover_by_name(
        &self,
        Parameters(req): Parameters<tools::lean::HoverByNameRequest>,
    ) -> std::result::Result<Json<Response<tools::lean::HoverByNameResult>>, McpError> {
        wrap(tools::lean::hover_by_name(&self.ctx, req).await)
    }

    #[tool(
        description = "Filesystem regex sweep over the project's .lean files. Presets: sorry, admit, axiom, set_option."
    )]
    async fn project_scan(
        &self,
        Parameters(req): Parameters<tools::scan::ProjectScanRequest>,
    ) -> std::result::Result<Json<Response<tools::scan::ProjectScanResult>>, McpError> {
        wrap(tools::scan::project_scan(&self.ctx, req))
    }
}

#[tool_handler]
impl ServerHandler for LeanHostService {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]` from rmcp — struct literal
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
             look up declarations by name, and scan .lean files."
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
