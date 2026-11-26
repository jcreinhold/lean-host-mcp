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
//!
//! Every reply names its request. A call with a `request_id` argument runs as
//! that request, recorded in the request table before it starts (see
//! [`crate::requests`]); any other call gets a `srv-` id and no row. A reply
//! carries the id at its top level, and a protocol error in its `data`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::broker::{ProjectBroker, ProjectHint, RuntimeControlRequest};
use crate::cancellation::{with_cancellation, with_deadline};
use crate::error::ServerError;
use crate::request_log::RequestLog;
use crate::requests::{self, REQUEST_ID, RequestError, RequestRunner, RowView};
use crate::runtime_compatibility::{CompatibilityMode, CompatibilityRequest};
use crate::tools::{self, ResponseCarrier, ToolConfig, ToolContext};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

/// Tools that never run as a request: the two that manage requests, and the
/// runtime lease, which must answer at once.
const DIRECT_ONLY: [&str; 3] = ["lean_run", "lean_get", "lean_runtime"];

/// Run one tool call while heartbeating `notifications/progress` to the client.
///
/// Every worker-backed tool can legitimately run longer than a client's idle
/// timeout: a cold import, a large file's elaboration, a proof-step batch.
/// Clients such as Claude Code extend their per-call deadline only while
/// progress notifications keep arriving, so the heartbeat is what keeps a slow
/// but healthy call from being abandoned at the client while the worker keeps
/// computing. Silent when the client sent no progress token.
async fn with_progress<T>(context: &RequestContext<RoleServer>, tool: &str, operation: impl Future<Output = T>) -> T {
    with_cancellation(&context.ct, progress_while_waiting(context, tool, operation)).await
}

async fn progress_while_waiting<T>(
    context: &RequestContext<RoleServer>,
    tool: &str,
    operation: impl Future<Output = T>,
) -> T {
    let Some(progress_token) = context.meta.get_progress_token() else {
        return operation.await;
    };

    let peer = context.peer.clone();
    let started = Instant::now();
    let mut progress = 0.0;
    if let Err(error) = peer
        .notify_progress(
            ProgressNotificationParam::new(progress_token.clone(), progress).with_message(format!("Preparing {tool}")),
        )
        .await
    {
        tracing::debug!(?error, tool, "could not send start progress");
    }

    tokio::pin!(operation);
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            result = &mut operation => {
                progress += 1.0;
                let elapsed = started.elapsed().as_secs_f64();
                if let Err(error) = peer
                    .notify_progress(
                        ProgressNotificationParam::new(progress_token, progress)
                            .with_total(progress)
                            .with_message(format!("{tool} finished ({elapsed:.1}s elapsed)")),
                    )
                    .await
                {
                    tracing::debug!(?error, tool, "could not send finish progress");
                }
                return result;
            }
            _ = ticker.tick() => {
                progress += 1.0;
                let elapsed = started.elapsed().as_secs();
                if let Err(error) = peer
                    .notify_progress(
                        ProgressNotificationParam::new(progress_token.clone(), progress)
                            .with_message(format!("{tool} still running ({elapsed}s elapsed)")),
                    )
                    .await
                {
                    tracing::debug!(?error, tool, "could not send heartbeat progress");
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeanHostService {
    ctx: ToolContext,
    tool_router: ToolRouter<Self>,
    /// The request table, when the server has a store.
    requests: Option<Arc<RequestRunner>>,
}

impl LeanHostService {
    pub fn new(broker: Arc<ProjectBroker>, config: ToolConfig) -> Self {
        let ctx = ToolContext { broker, config };
        Self {
            ctx,
            tool_router: Self::tool_router(),
            requests: None,
        }
    }

    /// Run calls that carry a `request_id` through `runner`. Without it such
    /// calls fail with `requests_unavailable`.
    #[must_use]
    pub fn with_requests(mut self, runner: Arc<RequestRunner>) -> Self {
        self.requests = Some(runner);
        self
    }
}

/// `lean_run` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeanRunRequest {
    /// Names the request: 8 to 128 characters from A-Z, a-z, 0-9, '.', '_', ':' and '-', not starting with `srv-`.
    pub request_id: String,
    /// The tool to run: any tool except `lean_run`, `lean_get` and `lean_runtime`.
    pub tool: String,
    /// The tool's arguments, without `request_id`.
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

/// `lean_get` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeanGetRequest {
    /// The request to read.
    pub request_id: String,
    /// Wait up to this many seconds for the request to end. The server caps the wait at its `[requests]
    /// wait_max_secs`. Default 0: reply at once.
    #[serde(default)]
    pub wait_seconds: Option<u64>,
}

/// The id `call_tool` gave a call that came without a `request_id`.
#[derive(Clone, Debug)]
struct CallId(String);

#[tool_router]
impl LeanHostService {
    #[tool(
        description = "Start a tool call as request request_id and reply at once with the request's row; read its result with lean_get. The server records the request before it starts and runs it at most once: the same request_id with the same tool and arguments returns the same row, from any client and after a restart; other arguments fail with request_id_conflict. tool is any tool except lean_run, lean_get and lean_runtime."
    )]
    async fn lean_run(
        &self,
        Parameters(request): Parameters<LeanRunRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let carrier = self.ctx.config.carrier;
        let Some(runner) = &self.requests else {
            return Ok(RequestError::unavailable().reply(&request.request_id, carrier));
        };
        let client = client_name(&context);
        Ok(
            match runner
                .run(&request.request_id, &request.tool, request.arguments, client)
                .await
            {
                Ok(row) => carry(&json!({ "request": RowView::from(&row) }), carrier),
                Err(error) => error.reply(&request.request_id, carrier),
            },
        )
    }

    #[tool(
        description = "Read request request_id: its row, and once it has ended, the stored result of its tool call (null before). wait_seconds waits up to that many seconds for the end; the server caps the wait."
    )]
    async fn lean_get(
        &self,
        Parameters(request): Parameters<LeanGetRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let carrier = self.ctx.config.carrier;
        let Some(runner) = &self.requests else {
            return Ok(RequestError::unavailable().reply(&request.request_id, carrier));
        };
        if let Err(error) = requests::check_id(&request.request_id) {
            return Ok(error.reply(&request.request_id, carrier));
        }
        let limit = runner.wait_limit(request.wait_seconds);
        let wait = progress_while_waiting(&context, "lean_get", runner.wait(&request.request_id, Some(limit)));
        let waited = tokio::select! {
            biased;
            () = context.ct.cancelled() => return Err(McpError::internal_error("the client stopped waiting", None)),
            waited = wait => waited,
        };
        Ok(match waited {
            Ok(row) => {
                let result = row
                    .state
                    .is_final()
                    .then(|| {
                        row.result
                            .as_deref()
                            .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    })
                    .flatten();
                carry(&json!({ "request": RowView::from(&row), "result": result }), carrier)
            }
            Err(error) => error.reply(&request.request_id, carrier),
        })
    }

    #[tool(
        description = "Describe compiled native requirements without work using mode=\"pure_v1\", or verify an actual loaded worker using the default mode=\"loaded\". Pure discovery never starts a worker or preparer."
    )]
    async fn lean_compatibility(
        &self,
        Parameters(request): Parameters<CompatibilityRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Compatibility(request), "lean_compatibility", &context)
            .await
    }

    #[tool(
        description = "Observe worker activity or atomically lease the idle worker pool. Actions: status, acquire, renew, release. Leases pause MCP admission; independent artifact writers may retire idle caches. Set retire_idle for explicit retirement and renew before expiry while relying on the pause."
    )]
    async fn lean_runtime(
        &self,
        Parameters(request): Parameters<RuntimeControlRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mut result = self.ctx.broker.runtime_control(&request);
        result.effective_settings.server = Some(self.ctx.config.into());
        if let Some(runner) = &self.requests {
            match runner.counts().await {
                Ok(counts) => result.requests = Some(counts),
                Err(error) => tracing::warn!(%error, "cannot count the requests"),
            }
            result.capabilities.push("durable_requests_v1".to_owned());
        }
        Ok(carry(&result, self.ctx.config.carrier))
    }

    #[tool(
        description = "Lean proof or term context. Use kind=\"proof_position\" for proof state at a declaration position."
    )]
    async fn lean_context(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanContextToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Semantic(SemanticCall::Context(req)), "lean_context", &context)
            .await
    }

    #[tool(
        description = "Lean experiments without automatic source edits. Kinds: proof_step, command. Runs trusted Lean code with server privileges, not in an OS sandbox."
    )]
    async fn lean_trial(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanTrialToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Semantic(SemanticCall::Trial(req)), "lean_trial", &context)
            .await
    }

    #[tool(description = "Verify Lean declarations from explicit, file_all, or module_all target groups.")]
    async fn lean_verify(
        &self,
        Parameters(req): Parameters<tools::proof_action::LeanVerifyToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Semantic(SemanticCall::Verify(req)), "lean_verify", &context)
            .await
    }

    #[tool(description = "Semantic lookup. Kinds: declaration, declarations, proof_search, references.")]
    async fn lean_lookup(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanLookupToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Semantic(SemanticCall::Lookup(req)), "lean_lookup", &context)
            .await
    }

    #[tool(
        description = "Run Lake in the project: build `targets` (none: Lake's default targets) or run a configured `recipe` with `args`. Checks wait while it runs. Replies with each step's exit code, the log path and the log's tail; isError when a step failed or the build timed out."
    )]
    async fn lean_build(
        &self,
        Parameters(req): Parameters<crate::build::BuildRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Build(req), "lean_build", &context).await
    }

    #[tool(
        description = "List each file's whole transitive import closure from Lake setup-file (importArts), not only its direct imports: module, absolute .olean path, and the absolute source path for modules built under this project's .lake/build (else null). Builds nothing: a missing or stale import fails the call with error code setup_file_failed, naming the file and quoting Lake. At most 16 files."
    )]
    async fn lean_imports(
        &self,
        Parameters(req): Parameters<tools::build::ImportsRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Imports(req), "lean_imports", &context).await
    }

    #[tool(description = "Project/toolchain status and source diagnostics. Kinds: project, file_diagnostics.")]
    async fn lean_status(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanStatusToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.direct(ToolCall::Semantic(SemanticCall::Status(req)), "lean_status", &context)
            .await
    }
}

impl LeanHostService {
    /// Run `call` for a call without a `request_id`, as the id `call_tool`
    /// gave it. A dropped connection cancels the work.
    async fn direct(
        &self,
        call: ToolCall,
        tool: &str,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let id = context
            .extensions
            .get::<CallId>()
            .map_or_else(requests::server_id, |id| id.0.clone());
        Box::pin(with_progress(context, tool, call.execute(&self.ctx, &id))).await
    }

    async fn route(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    /// Run a call that names request `id` and wait for its end. A dropped
    /// connection ends the wait, not the request.
    async fn run_named(
        &self,
        id: &str,
        tool: &str,
        arguments: Map<String, Value>,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let carrier = self.ctx.config.carrier;
        if self.tool_router.get(tool).is_none() {
            let mut error = McpError::invalid_params("tool not found", None);
            requests::stamp_error(&mut error, id);
            return Err(error);
        }
        let Some(runner) = &self.requests else {
            return Ok(RequestError::unavailable().reply(id, carrier).into());
        };
        if let Err(error) = runner.run(id, tool, arguments, client_name(context)).await {
            return Ok(error.reply(id, carrier).into());
        }
        let waited = tokio::select! {
            biased;
            () = context.ct.cancelled() => {
                let mut error = McpError::internal_error("the client stopped waiting; the request goes on", None);
                requests::stamp_error(&mut error, id);
                return Err(error);
            }
            waited = progress_while_waiting(context, tool, runner.wait(id, None)) => waited,
        };
        Ok(match waited {
            Ok(row) => runner.stored_reply(&row),
            Err(error) => error.reply(id, carrier),
        }
        .into())
    }
}

impl ServerHandler for LeanHostService {
    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        mut context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let tool = request.name.to_string();
        if matches!(tool.as_str(), "lean_run" | "lean_get") {
            // These tools name their request in their own arguments.
            let id = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get(REQUEST_ID))
                .and_then(Value::as_str)
                .filter(|id| requests::check_id(id).is_ok())
                .map_or_else(requests::server_id, str::to_owned);
            return stamped(self.route(request, context).await, &id);
        }
        let Some(given) = request
            .arguments
            .as_mut()
            .and_then(|arguments| arguments.remove(REQUEST_ID))
        else {
            let id = requests::server_id();
            context.extensions.insert(CallId(id.clone()));
            return stamped(self.route(request, context).await, &id);
        };
        match requests::id_from(&given) {
            Ok(id) => {
                let arguments = request.arguments.take().unwrap_or_default();
                self.run_named(&id, &tool, arguments, &context).await
            }
            Err(error) => Ok(error.reply(&requests::server_id(), self.ctx.config.carrier).into()),
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = self.tool_router.list_all().into_iter().map(with_request_id).collect();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned().map(with_request_id)
    }

    fn get_info(&self) -> ServerConfig {
        // `ServerConfig` is `#[non_exhaustive]` from rmcp; struct literal
        // syntax (even with `..default`) is forbidden across crates. Build
        // via Default + field mutation.
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_website_url("https://github.com/jcreinhold/lean-host-mcp");
        let common = "MCP server hosting Lean 4 in supervised worker children via lean-rs. \
             Tools expose five semantic Lean job families: context, trial, verification, lookup, and status. \
             Context, trial, lookup, and status select a mode with `kind`; verification takes explicit target groups. \
             lean_build runs Lake under the project's write turn, so checks wait for it; lean_imports reads what Lake built.";
        let requests = " Add request_id to a call to make it a named request: the server records it before it starts, \
             runs it at most once and keeps its result, so a retry with the same id returns that result. \
             lean_run starts a named request and replies at once; lean_get reads its row and result.";
        info.instructions = Some(format!("{common}{requests}"));
        info
    }
}

/// The client's name and version from `initialize`, if it sent them.
fn client_name(context: &RequestContext<RoleServer>) -> Option<String> {
    context
        .client_info()
        .map(|info| format!("{} {}", info.name, info.version))
}

/// `response` with request `id` at the top of its result or in its error data.
fn stamped(response: Result<CallToolResponse, McpError>, id: &str) -> Result<CallToolResponse, McpError> {
    match response {
        Ok(mut response) => {
            if let CallToolResponse::Complete(result) = &mut response {
                requests::stamp_result(result, id);
            }
            Ok(response)
        }
        Err(mut error) => {
            requests::stamp_error(&mut error, id);
            Err(error)
        }
    }
}

/// `tool` with the optional `request_id` argument in its input schema, unless
/// it never runs as a request.
fn with_request_id(mut tool: Tool) -> Tool {
    if DIRECT_ONLY.contains(&tool.name.as_ref()) {
        return tool;
    }
    let schema = Arc::make_mut(&mut tool.input_schema);
    let properties = schema.entry("properties").or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(properties) = properties {
        properties.insert(
            REQUEST_ID.to_owned(),
            json!({
                "type": "string",
                "pattern": "^[A-Za-z0-9._:-]{8,128}$",
                "description": "Run the call as this named request. The server records it before it starts and runs it at most once: a retry with the same id and arguments returns the stored result. Ids that start with srv- belong to the server.",
            }),
        );
    }
    tool
}

/// A parsed call of a tool that can run as a request.
pub(crate) enum ToolCall {
    /// One of the five semantic tools.
    Semantic(SemanticCall),
    Build(crate::build::BuildRequest),
    Imports(tools::build::ImportsRequest),
    Compatibility(CompatibilityRequest),
}

/// A call of one of the semantic tools.
pub(crate) enum SemanticCall {
    Context(tools::semantic::LeanContextToolRequest),
    Trial(tools::semantic::LeanTrialToolRequest),
    Verify(tools::proof_action::LeanVerifyToolRequest),
    Lookup(tools::semantic::LeanLookupToolRequest),
    Status(tools::semantic::LeanStatusToolRequest),
}

/// Why a tool call cannot run as a request.
#[derive(Debug)]
pub(crate) enum ToolCallError {
    /// No tool has the name, or the tool does not run as a request.
    UnknownTool(String),
    /// The tool refuses the arguments.
    InvalidArguments(String),
}

impl ToolCall {
    /// Decode and check the arguments of `tool`.
    ///
    /// # Errors
    /// Names a tool that is unknown or does not run as a request, or the fault
    /// in the arguments.
    pub(crate) fn parse(tool: &str, arguments: &Map<String, Value>) -> Result<Self, ToolCallError> {
        let args = Value::Object(arguments.clone());
        let parsed = match tool {
            "lean_context" => serde_json::from_value(args).map(|req| Self::Semantic(SemanticCall::Context(req))),
            "lean_trial" => serde_json::from_value(args).map(|req| Self::Semantic(SemanticCall::Trial(req))),
            "lean_verify" => serde_json::from_value(args).map(|req| Self::Semantic(SemanticCall::Verify(req))),
            "lean_lookup" => serde_json::from_value(args).map(|req| Self::Semantic(SemanticCall::Lookup(req))),
            "lean_status" => serde_json::from_value(args).map(|req| Self::Semantic(SemanticCall::Status(req))),
            "lean_build" => serde_json::from_value(args).map(Self::Build),
            "lean_imports" => serde_json::from_value(args).map(Self::Imports),
            "lean_compatibility" => serde_json::from_value(args).map(Self::Compatibility),
            _ if DIRECT_ONLY.contains(&tool) => {
                return Err(ToolCallError::UnknownTool(format!(
                    "{tool} does not run as a request; call it directly"
                )));
            }
            _ => return Err(ToolCallError::UnknownTool(format!("no tool is named {tool}"))),
        };
        let call = parsed
            .map_err(|error| ToolCallError::InvalidArguments(format!("failed to deserialize parameters: {error}")))?;
        call.check().map_err(ToolCallError::InvalidArguments)?;
        Ok(call)
    }

    /// Refuse arguments that decode but that the tool never accepts.
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Compatibility(request) if request.project.trim().is_empty() => {
                Err("explicit project required".to_owned())
            }
            Self::Imports(request)
                if request.files.is_empty() || request.files.len() > tools::build::MAX_IMPORT_FILES =>
            {
                Err(format!("`files` needs 1 to {} files", tools::build::MAX_IMPORT_FILES))
            }
            Self::Semantic(_) | Self::Build(_) | Self::Imports(_) | Self::Compatibility(_) => Ok(()),
        }
    }

    /// The `project` argument, if any.
    pub(crate) fn project(&self) -> Option<String> {
        match self {
            Self::Semantic(call) => call.project(),
            Self::Build(request) => request.project.clone(),
            Self::Imports(request) => request.project.clone(),
            Self::Compatibility(request) => Some(request.project.clone()),
        }
    }

    /// Whether the call is one of the semantic tools, whose failures are
    /// semantic responses.
    pub(crate) const fn is_semantic(&self) -> bool {
        matches!(self, Self::Semantic(_))
    }

    /// Run the call as request or call `id`, which names a build and its log.
    ///
    /// # Errors
    /// Returns the tool's protocol error.
    pub(crate) async fn execute(self, ctx: &ToolContext, id: &str) -> Result<CallToolResult, McpError> {
        self.check()
            .map_err(|message| McpError::invalid_params(message, None))?;
        let carrier = ctx.config.carrier;
        // One deadline covers the wait for a project turn and the work after it.
        let timeout = Duration::from_millis(ctx.broker.request_timeout_millis());
        match self {
            Self::Semantic(call) => {
                let mut log = call.request_log();
                // The box keeps this future small; the work's own future is large.
                let result = with_deadline(timeout, Box::pin(call.run(ctx))).await;
                log.finish(&result);
                respond_semantic(result, carrier)
            }
            Self::Build(request) => {
                let outcome = tools::build::lean_build_as(ctx, request, id).await?;
                let mut result = carry(&outcome, carrier);
                result.is_error = Some(!outcome.ok);
                Ok(result)
            }
            Self::Imports(request) => match with_deadline(timeout, tools::build::lean_imports(ctx, request)).await {
                Ok(reply) => Ok(carry(&reply, carrier)),
                Err(tools::build::ImportsError::File(failure)) => {
                    let mut result = carry(&failure, carrier);
                    result.is_error = Some(true);
                    Ok(result)
                }
                Err(tools::build::ImportsError::Call(error)) => Err(error),
            },
            Self::Compatibility(request) => {
                let hint = ProjectHint::Explicit(PathBuf::from(request.project));
                if matches!(request.mode, CompatibilityMode::PureV1) {
                    let result = ctx
                        .broker
                        .describe_native_compatibility(&hint)
                        .map_err(McpError::from)?;
                    return Ok(carry(&result, carrier));
                }
                let result = ctx.broker.native_compatibility(hint).await.map_err(McpError::from)?;
                Ok(carry(&result, carrier))
            }
        }
    }
}

impl SemanticCall {
    fn project(&self) -> Option<String> {
        let value = match self {
            Self::Context(req) => req.0.args.get("project"),
            Self::Trial(req) => req.0.args.get("project"),
            Self::Lookup(req) => req.0.args.get("project"),
            Self::Status(req) => req.0.args.get("project"),
            Self::Verify(req) => req.as_value().get("project"),
        };
        value.and_then(Value::as_str).map(str::to_owned)
    }

    fn request_log(&self) -> RequestLog {
        match self {
            Self::Context(req) => semantic_log("lean_context", &req.0),
            Self::Trial(req) => semantic_log("lean_trial", &req.0),
            Self::Lookup(req) => semantic_log("lean_lookup", &req.0),
            Self::Status(req) => semantic_log("lean_status", &req.0),
            Self::Verify(req) => RequestLog::new(
                "lean_verify",
                None,
                req.as_value().get("project").and_then(Value::as_str),
            ),
        }
    }

    async fn run(self, ctx: &ToolContext) -> crate::error::Result<tools::semantic::SemanticResponse<Value>> {
        match self {
            Self::Context(req) => tools::semantic::lean_context(ctx, req.into_inner()).await,
            Self::Trial(req) => tools::semantic::lean_trial(ctx, req.into_inner()).await,
            Self::Verify(req) => tools::semantic::lean_verify_raw(ctx, req.into_inner()).await,
            Self::Lookup(req) => tools::semantic::lean_lookup(ctx, req.into_inner()).await,
            Self::Status(req) => tools::semantic::lean_status(ctx, req.into_inner()).await,
        }
    }
}

fn semantic_log(tool: &'static str, req: &tools::semantic::SemanticToolRequest) -> RequestLog {
    RequestLog::new(
        tool,
        req.kind.as_deref(),
        req.args.get("project").and_then(Value::as_str),
    )
}

/// A semantic tool's result, or its failure as a semantic response when the
/// worker is unavailable or incompatible.
///
/// # Errors
/// Returns the protocol error of any other failure.
pub(crate) fn respond_semantic(
    result: crate::error::Result<tools::semantic::SemanticResponse<Value>>,
    carrier: ResponseCarrier,
) -> Result<CallToolResult, McpError> {
    let response = match result {
        Ok(response) => response,
        Err(ServerError::WorkerUnavailable(info)) => tools::semantic::from_worker_unavailable(&info),
        Err(ServerError::IncompatibleWorker {
            message,
            recovery_command,
        }) => tools::semantic::from_incompatible_worker(&message, &recovery_command),
        Err(err) => return Err(McpError::from(err)),
    };
    Ok(carry_semantic(&response, carrier))
}

/// Invocation/runtime failures remain structured tool results, with the MCP
/// error flag set. Negative Lean proof/check verdicts still carry domain data
/// and are successful tool executions, not protocol or invocation failures.
fn carry_semantic(response: &tools::semantic::SemanticResponse<Value>, carrier: ResponseCarrier) -> CallToolResult {
    let mut result = carry(response, carrier);
    if response.data.is_none()
        && response
            .errors
            .iter()
            .any(|issue| issue.severity.as_deref() == Some("error"))
    {
        result.is_error = Some(true);
    }
    result
}

/// Serialize the response and place it in the tool result per the carrier:
/// `Text` emits one `content` text block (the model's read surface), `Both`
/// also mirrors it into `structuredContent` for code-mode clients, and
/// `Structured` emits only `structuredContent`.
pub(crate) fn carry<T>(response: &T, carrier: ResponseCarrier) -> CallToolResult
where
    T: serde::Serialize + schemars::JsonSchema,
{
    let value = match serde_json::to_value(response) {
        Ok(value) => value,
        Err(err) => {
            return CallToolResult::error(vec![ContentBlock::text(format!("failed to serialize response: {err}"))]);
        }
    };
    match carrier {
        ResponseCarrier::Text => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
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
            worker_stop_receipt: None,
            preparation_closed: false,
            project_root: "/tmp/project".to_owned(),
            project_hash: "hash".to_owned(),
            imports: vec!["Init".to_owned()],
            session_id: "session".to_owned(),
            lean_toolchain: "leanprover/lean4:v4.30.0".to_owned(),
            worker_generation: 7,
            reason: "worker_death".to_owned(),
            blocked_by: None,
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

    #[test]
    fn incompatible_worker_is_nonretryable_with_exact_recovery_command() {
        let response = tools::semantic::from_incompatible_worker(
            "worker protocol 1 is incompatible with host protocol 2",
            "lean-host-mcp install-worker --auto",
        );
        assert_eq!(carry_semantic(&response, ResponseCarrier::Both).is_error, Some(true));
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json.pointer("/errors/0/code").and_then(serde_json::Value::as_str),
            Some("runtime_unavailable")
        );
        assert_eq!(
            json.pointer("/errors/0/retryable").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            json.pointer("/errors/0/next_action")
                .and_then(serde_json::Value::as_str),
            Some("lean-host-mcp install-worker --auto")
        );
        assert_eq!(
            json.pointer("/errors/0/details/recovery_command")
                .and_then(serde_json::Value::as_str),
            Some("lean-host-mcp install-worker --auto")
        );
    }

    #[test]
    fn semantic_error_flag_preserves_domain_data_and_all_carriers() {
        for carrier in [
            ResponseCarrier::Text,
            ResponseCarrier::Both,
            ResponseCarrier::Structured,
        ] {
            let mut response = tools::semantic::from_incompatible_worker("unavailable worker", "install worker");
            assert_eq!(carry_semantic(&response, carrier).is_error, Some(true));
            response.data = Some(serde_json::json!({"verdict": "rejected"}));
            assert_ne!(carry_semantic(&response, carrier).is_error, Some(true));
            response.data = None;
            response.errors.first_mut().unwrap().severity = Some("warning".to_owned());
            assert_ne!(carry_semantic(&response, carrier).is_error, Some(true));
        }
    }

    #[test]
    fn carriers_preserve_actionable_metadata() {
        let response = serde_json::json!({
            "data": {
                "proof_boundaries": [{
                    "index": 1,
                    "selector": { "kind": "index", "index": 1 },
                    "excerpt": { "value": "exact h", "truncated": false }
                }],
                "candidates": [{
                    "diagnostics": {
                        "diagnostics": [{
                            "message": "unknown identifier",
                            "coordinate_space": "synthetic_buffer",
                            "synthetic_range": { "line": 82, "column": 3 }
                        }]
                    }
                }]
            },
            "errors": [],
            "trust": {
                "project_root": "/tmp/project",
                "session_id": "session",
                "lean_toolchain": "leanprover/lean4:v4.35.0"
            }
        });

        let text = serde_json::to_value(carry(&response, ResponseCarrier::Text)).unwrap();
        let text_blob = text.to_string();
        assert!(text_blob.contains("proof_boundaries"));
        assert!(text_blob.contains("synthetic_buffer"));

        let structured = serde_json::to_value(carry(&response, ResponseCarrier::Structured)).unwrap();
        assert_eq!(
            structured
                .pointer("/structuredContent/data/proof_boundaries/0/selector/kind")
                .and_then(serde_json::Value::as_str),
            Some("index")
        );
        assert_eq!(
            structured
                .pointer("/structuredContent/data/candidates/0/diagnostics/diagnostics/0/coordinate_space")
                .and_then(serde_json::Value::as_str),
            Some("synthetic_buffer")
        );

        let both = serde_json::to_value(carry(&response, ResponseCarrier::Both)).unwrap();
        assert_eq!(
            both.pointer("/structuredContent/data/proof_boundaries/0/index")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert!(
            both.to_string().contains("synthetic_buffer"),
            "both carrier should keep metadata on the model-visible content path too: {both:?}"
        );
    }
}
