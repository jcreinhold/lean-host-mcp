//! rmcp server glue. Registers fourteen tools and wires them to the
//! [`tools`](crate::tools) module.
//!
//! Each `#[tool]` handler is a thin call into the implementation function;
//! all real work happens in `crate::tools` and `crate::session`. Returns
//! `Json<Response<T>>` so rmcp generates structured-content output and a
//! schema downstream clients can introspect.

use std::num::NonZeroUsize;
use std::sync::Arc;

use rmcp::Json;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::cache::ProcessedFileCache;
use crate::envelope::Response;
use crate::index::DeclarationIndex;
use crate::session::SessionHost;
use crate::tools::{self, ToolContext};

/// LRU capacity for the in-memory `ProcessedFile` cache. Sized for a normal
/// multi-file proof session — large enough that twenty cursor moves across
/// a handful of files all hit, small enough to keep memory bounded.
const PROCESSED_FILE_CACHE_CAPACITY: usize = 16;

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
    pub fn new(host: SessionHost, index: Arc<DeclarationIndex>) -> Self {
        // The constant is non-zero by construction; `NonZeroUsize::MIN`
        // ([`NonZeroUsize::new(1)`]) is a safe fallback the type system
        // can verify, so we do not need an `unwrap` here.
        #[allow(
            clippy::missing_const_for_fn,
            reason = "NonZeroUsize::new is const but `or` is not yet on stable for NonZeroUsize"
        )]
        let cache_cap = NonZeroUsize::new(PROCESSED_FILE_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        let ctx = ToolContext {
            lake_root: host.lake_root().to_owned(),
            default_imports: Vec::new(),
            processed_files: Arc::new(ProcessedFileCache::with_capacity(cache_cap)),
            host,
            index,
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

    #[tool(
        description = "Find declarations by name (case-insensitive substring). Rebuilds the index when the Lake manifest changes."
    )]
    async fn find_symbol(
        &self,
        Parameters(req): Parameters<tools::index::FindSymbolRequest>,
    ) -> std::result::Result<Json<Response<Vec<crate::index::IndexedDeclaration>>>, McpError> {
        wrap(tools::index::find_symbol(&self.ctx, req).await)
    }

    #[tool(description = "Find theorems by name (case-insensitive substring); kind is restricted to `theorem`.")]
    async fn find_lemma(
        &self,
        Parameters(req): Parameters<tools::index::FindLemmaRequest>,
    ) -> std::result::Result<Json<Response<Vec<crate::index::IndexedDeclaration>>>, McpError> {
        wrap(tools::index::find_lemma(&self.ctx, req).await)
    }

    #[tool(
        description = "List declarations by fully-qualified name prefix. Omit `module_prefix` to walk the full table."
    )]
    async fn outline(
        &self,
        Parameters(req): Parameters<tools::index::OutlineRequest>,
    ) -> std::result::Result<Json<Response<Vec<crate::index::IndexedDeclaration>>>, McpError> {
        wrap(tools::index::outline(&self.ctx, req).await)
    }

    #[tool(
        description = "Proof goal at a cursor (1-indexed line/column) in a .lean file. Returns Goal / NoTacticContext / Unsupported."
    )]
    async fn goal_at_position(
        &self,
        Parameters(req): Parameters<tools::position::GoalAtPositionRequest>,
    ) -> std::result::Result<Json<Response<tools::position::GoalAtPositionResult>>, McpError> {
        wrap(tools::position::goal_at_position(&self.ctx, req).await)
    }

    #[tool(
        description = "Type and expected type of the innermost term at a cursor in a .lean file. Returns Term / NoTerm / Unsupported."
    )]
    async fn type_at_position(
        &self,
        Parameters(req): Parameters<tools::position::TypeAtPositionRequest>,
    ) -> std::result::Result<Json<Response<tools::position::TypeAtPositionResult>>, McpError> {
        wrap(tools::position::type_at_position(&self.ctx, req).await)
    }

    #[tool(
        description = "All binder and use-site occurrences of a fully-qualified Lean name across one or many .lean files (defaults to all project files)."
    )]
    async fn references_of_name(
        &self,
        Parameters(req): Parameters<tools::position::ReferencesOfNameRequest>,
    ) -> std::result::Result<Json<Response<tools::position::ReferencesOfNameResult>>, McpError> {
        wrap(tools::position::references_of_name(&self.ctx, req).await)
    }

    #[tool(
        description = "Elaboration diagnostics for a .lean file (errors, warnings, info). Returns Ok / HeaderParseFailed / Unsupported."
    )]
    async fn file_diagnostics(
        &self,
        Parameters(req): Parameters<tools::position::FileDiagnosticsRequest>,
    ) -> std::result::Result<Json<Response<tools::position::FileDiagnosticsResult>>, McpError> {
        wrap(tools::position::file_diagnostics(&self.ctx, req).await)
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
             look up declarations by name, scan .lean files, and answer \
             cursor-driven goal / type / references queries."
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
