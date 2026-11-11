//! rmcp server glue. Registers model-facing Lean tools and wires them to the
//! [`tools`] module.
//!
//! Each `#[tool]` handler is a thin call into the implementation function;
//! all real work happens in `crate::tools` and `crate::project`. Handlers
//! return a hand-built [`CallToolResult`] rather than `Json<T>`: the
//! `Json` wrapper makes rmcp advertise a deep `outputSchema` that no Anthropic
//! API client forwards to the model (it costs only wire bytes and breaks strict
//! schema validators), so we drop it and place the serialized envelope per the
//! configured [`ResponseCarrier`]. Clients negotiating the MCP Tasks extension
//! receive process-local task handles. Clone a service across transport sessions
//! to retain those handles and their results after a client reconnects.

use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams, ContentBlock, CreateTaskResult,
    GetTaskParams, GetTaskResult, Implementation, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ServerCapabilities, ServerConfig, Tool, UpdateTaskParams,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_router};
use sha2::{Digest, Sha256};

use crate::broker::{ProjectBroker, RuntimeControlRequest};
use crate::cancellation::with_cancellation;
use crate::error::ServerError;
use crate::request_log::RequestLog;
use crate::tasks::{RequestBinding, RequestTasks};
use crate::tools::{self, ResponseCarrier, ToolConfig, ToolContext};

const PROGRESS_INTERVAL: Duration = Duration::from_secs(15);

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
    tasks: RequestTasks,
}

impl LeanHostService {
    pub fn new(broker: Arc<ProjectBroker>, config: ToolConfig) -> Self {
        let ctx = ToolContext { broker, config };
        Self {
            ctx,
            tool_router: Self::tool_router(),
            tasks: RequestTasks::default(),
        }
    }
}

/// A plain-tool bridge for clients without the negotiated MCP Tasks extension.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RetainedRequest {
    action: RetainedAction,
    /// Existing ID for get/cancel; optional client-chosen ID for retry-safe submit.
    request_id: Option<String>,
    /// One of `lean_context`, `lean_trial`, `lean_verify`, `lean_lookup` or `lean_status`.
    tool: Option<String>,
    /// The selected semantic tool's ordinary input object.
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RetainedAction {
    Submit,
    Get,
    Cancel,
}

#[tool_router]
impl LeanHostService {
    #[tool(
        description = "Submit, retrieve or cancel a retained Lean request without MCP Tasks negotiation. action=submit takes tool and arguments and returns a taskId immediately. Use that ID as request_id for get/cancel. Supply a unique request_id on submit to retry a lost receipt without duplicate work. Records last ten minutes after completion, only in this server process."
    )]
    async fn lean_request(&self, Parameters(request): Parameters<RetainedRequest>) -> Result<CallToolResult, McpError> {
        let task = match request.action {
            RetainedAction::Submit => {
                let tool = request
                    .tool
                    .ok_or_else(|| McpError::invalid_params("submit requires tool", None))?;
                let call =
                    CallToolRequestParams::new(tool.clone()).with_arguments(request.arguments.unwrap_or_default());
                let parsed = TaskCall::parse(&call)?;
                let digest = Sha256::digest(
                    serde_json::to_vec(&(&tool, &call.arguments))
                        .map_err(|error| McpError::internal_error(error.to_string(), None))?,
                );
                let mut submission_sha256 = String::with_capacity(64);
                for byte in &digest {
                    let _ = write!(submission_sha256, "{byte:02x}");
                }
                let submission = RequestBinding {
                    server_instance_id: self.ctx.broker.instance_id(),
                    tool: tool.clone(),
                    submission_sha256,
                    project: call
                        .arguments
                        .as_ref()
                        .and_then(|args| args.get("project"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                };
                let request_id = request.request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let service = self.clone();
                let accepted = self.tasks.submit(&tool, request_id, submission, move || async move {
                    service.run_task(parsed).await
                })?;
                self.tasks.get_retained(&accepted.task_id)?
            }
            RetainedAction::Get | RetainedAction::Cancel => {
                if request.tool.is_some() || request.arguments.is_some() {
                    return Err(McpError::invalid_params("get/cancel accepts only request_id", None));
                }
                let request_id = request
                    .request_id
                    .ok_or_else(|| McpError::invalid_params("get/cancel requires request_id", None))?;
                if matches!(request.action, RetainedAction::Cancel) {
                    self.tasks.cancel(&request_id)?;
                }
                self.tasks.get_retained(&request_id)?
            }
        };
        let value = serde_json::to_value(task).map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(carry(&value, self.ctx.config.carrier))
    }

    #[tool(
        description = "Observe worker activity or atomically lease the idle worker pool. Actions: status, acquire, renew, release. Leases keep warm caches unless retire_idle is explicitly true; renew before expiry while relying on the pause."
    )]
    async fn lean_runtime(
        &self,
        Parameters(request): Parameters<RuntimeControlRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mut result = self.ctx.broker.runtime_control(&request);
        result.effective_settings.server = Some(self.ctx.config.into());
        result.request_registry = Some(self.tasks.status());
        Ok(carry(&result, self.ctx.config.carrier))
    }

    #[tool(
        description = "Lean proof or term context. Use kind=\"proof_position\" for proof state at a declaration position."
    )]
    async fn lean_context(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanContextToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Box::pin(with_progress(
            &context,
            "lean_context",
            self.run_task(TaskCall::Context(req)),
        ))
        .await
    }

    #[tool(
        description = "Lean experiments without automatic source edits. Kinds: proof_step, command. Runs trusted Lean code with server privileges, not in an OS sandbox."
    )]
    async fn lean_trial(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanTrialToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Box::pin(with_progress(
            &context,
            "lean_trial",
            self.run_task(TaskCall::Trial(req)),
        ))
        .await
    }

    #[tool(description = "Verify Lean declarations from explicit, file_all, or module_all target groups.")]
    async fn lean_verify(
        &self,
        Parameters(req): Parameters<tools::proof_action::LeanVerifyToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Box::pin(with_progress(
            &context,
            "lean_verify",
            self.run_task(TaskCall::Verify(req)),
        ))
        .await
    }

    #[tool(description = "Semantic lookup. Kinds: declaration, declarations, proof_search, references.")]
    async fn lean_lookup(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanLookupToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Box::pin(with_progress(
            &context,
            "lean_lookup",
            self.run_task(TaskCall::Lookup(req)),
        ))
        .await
    }

    #[tool(description = "Project/toolchain status and source diagnostics. Kinds: project, file_diagnostics.")]
    async fn lean_status(
        &self,
        Parameters(req): Parameters<tools::semantic::LeanStatusToolRequest>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        Box::pin(with_progress(
            &context,
            "lean_status",
            self.run_task(TaskCall::Status(req)),
        ))
        .await
    }
}

impl ServerHandler for LeanHostService {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // Coordination is immediate and must return its lease token directly,
        // even for clients that negotiate asynchronous Lean job handles.
        if !matches!(request.name.as_ref(), "lean_runtime" | "lean_request")
            && context.client_capabilities().is_some_and(|caps| caps.supports_tasks())
        {
            // Parse before acceptance. Task calls use no request-scoped peer or
            // cancellation token, so disconnecting cannot abandon their result.
            // Let the router handle parse failures so malformed arguments have
            // the same tool-error shape with and without task negotiation.
            if let Ok(call) = TaskCall::parse(&request)
                && !call.is_cheap_metadata()
            {
                let service = self.clone();
                if let Some(task) = self
                    .tasks
                    .spawn(&request.name, move || async move { service.run_task(call).await })
                {
                    return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
                }
            }
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.tool_router.list_all())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetTaskResult, McpError>> + Send + '_ {
        std::future::ready(self.tasks.get(&request.task_id).map(GetTaskResult::new))
    }

    fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        // These tools never ask for in-task input. Unknown/already answered
        // input keys are ignored, as required by the Tasks extension.
        std::future::ready(self.tasks.get(&request.task_id).map(|_| ()))
    }

    fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        std::future::ready(self.tasks.cancel(&request.task_id))
    }

    fn get_info(&self) -> ServerConfig {
        // `ServerConfig` is `#[non_exhaustive]` from rmcp; struct literal
        // syntax (even with `..default`) is forbidden across crates. Build
        // via Default + field mutation.
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().enable_tasks().build();
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_website_url("https://github.com/jcreinhold/lean-host-mcp");
        info.instructions = Some(
            "MCP server hosting Lean 4 in-process via lean-rs. \
             Tools expose five semantic Lean job families: context, trial, \
             verification, lookup, and status. Context, trial, lookup, and \
             status select a mode with `kind`; verification takes explicit \
             target groups. Clients advertising io.modelcontextprotocol/tasks \
             may receive task handles; results remain available for ten minutes \
             after completion while this server process lives. Use lean_request to submit, \
             retrieve or cancel retained requests without task negotiation."
                .to_owned(),
        );
        info
    }
}

impl LeanHostService {
    async fn run_task(&self, call: TaskCall) -> Result<CallToolResult, McpError> {
        let mut log = call.request_log();
        let result = match call {
            TaskCall::Context(req) => tools::semantic::lean_context(&self.ctx, req.into_inner()).await,
            TaskCall::Trial(req) => tools::semantic::lean_trial(&self.ctx, req.into_inner()).await,
            TaskCall::Verify(req) => tools::semantic::lean_verify_raw(&self.ctx, req.into_inner()).await,
            TaskCall::Lookup(req) => tools::semantic::lean_lookup(&self.ctx, req.into_inner()).await,
            TaskCall::Status(req) => tools::semantic::lean_status(&self.ctx, req.into_inner()).await,
        };
        log.finish(&result);
        self.respond_semantic(result)
    }

    fn respond_semantic(
        &self,
        result: crate::error::Result<tools::semantic::SemanticResponse<serde_json::Value>>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let response = match result {
            Ok(response) => response,
            Err(ServerError::WorkerUnavailable(info)) => tools::semantic::from_worker_unavailable(&info),
            Err(ServerError::IncompatibleWorker {
                message,
                recovery_command,
            }) => tools::semantic::from_incompatible_worker(&message, &recovery_command),
            Err(err) => return Err(McpError::from(err)),
        };
        Ok(carry_semantic(&response, self.ctx.config.carrier))
    }
}

enum TaskCall {
    Context(tools::semantic::LeanContextToolRequest),
    Trial(tools::semantic::LeanTrialToolRequest),
    Verify(tools::proof_action::LeanVerifyToolRequest),
    Lookup(tools::semantic::LeanLookupToolRequest),
    Status(tools::semantic::LeanStatusToolRequest),
}

impl TaskCall {
    fn request_log(&self) -> RequestLog {
        match self {
            Self::Context(req) => Self::semantic_log("lean_context", &req.0),
            Self::Trial(req) => Self::semantic_log("lean_trial", &req.0),
            Self::Lookup(req) => Self::semantic_log("lean_lookup", &req.0),
            Self::Status(req) => Self::semantic_log("lean_status", &req.0),
            Self::Verify(req) => RequestLog::new(
                "lean_verify",
                None,
                req.as_value().get("project").and_then(serde_json::Value::as_str),
            ),
        }
    }

    fn semantic_log(tool: &'static str, req: &tools::semantic::SemanticToolRequest) -> RequestLog {
        RequestLog::new(
            tool,
            req.kind.as_deref(),
            req.args.get("project").and_then(serde_json::Value::as_str),
        )
    }

    fn is_cheap_metadata(&self) -> bool {
        matches!(self, Self::Status(req) if tools::request_aliases::canonical_kind(
            "lean_status",
            req.0.kind.as_deref().map(str::trim).filter(|kind| !kind.is_empty()).unwrap_or("project"),
        ) == "project")
    }

    fn parse(request: &CallToolRequestParams) -> Result<Self, McpError> {
        let args = serde_json::Value::Object(request.arguments.clone().unwrap_or_default());
        let result = match request.name.as_ref() {
            "lean_context" => serde_json::from_value(args).map(Self::Context),
            "lean_trial" => serde_json::from_value(args).map(Self::Trial),
            "lean_verify" => serde_json::from_value(args).map(Self::Verify),
            "lean_lookup" => serde_json::from_value(args).map(Self::Lookup),
            "lean_status" => serde_json::from_value(args).map(Self::Status),
            _ => return Err(McpError::invalid_params("tool not found", None)),
        };
        result.map_err(|error| McpError::invalid_params(error.to_string(), None))
    }
}

/// Invocation/runtime failures remain structured tool results, with the MCP
/// error flag set. Negative Lean proof/check verdicts still carry domain data
/// and are successful tool executions, not protocol or invocation failures.
fn carry_semantic(
    response: &tools::semantic::SemanticResponse<serde_json::Value>,
    carrier: ResponseCarrier,
) -> CallToolResult {
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
fn carry<T>(response: &T, carrier: ResponseCarrier) -> CallToolResult
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
    use rmcp::ServiceExt;
    use rmcp::model::{ClientCapabilities, NumberOrString, TaskPayload, TaskStatus};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn retained_duplicate_submissions_and_polls_log_only_actual_execution() {
        let (_guard, buffer) = crate::request_log::tests::capture();
        let dir = tempfile::tempdir().unwrap();
        let service = LeanHostService::new(
            ProjectBroker::new(crate::broker::BrokerConfig {
                config_default: Some(dir.path().to_path_buf()),
                env_default: None,
                cwd: dir.path().to_path_buf(),
                max_projects: crate::broker::BrokerConfig::default_max_projects(),
                idle_timeout: Duration::ZERO,
            }),
            ToolConfig::default(),
        );
        let submit = || RetainedRequest {
            action: RetainedAction::Submit,
            request_id: Some("SECRET_CLIENT_ID".to_owned()),
            tool: Some("lean_trial".to_owned()),
            arguments: Some(
                serde_json::from_value(serde_json::json!({"kind":"SECRET_KIND","commands":"SECRET_PROOF"})).unwrap(),
            ),
        };
        service.lean_request(Parameters(submit())).await.unwrap();
        service.lean_request(Parameters(submit())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !service.tasks.get("SECRET_CLIENT_ID").unwrap().status().is_terminal() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for _ in 0..3 {
            service
                .lean_request(Parameters(RetainedRequest {
                    action: RetainedAction::Get,
                    request_id: Some("SECRET_CLIENT_ID".to_owned()),
                    tool: None,
                    arguments: None,
                }))
                .await
                .unwrap();
        }
        service
            .lean_runtime(Parameters(RuntimeControlRequest::default()))
            .await
            .unwrap();
        let output = buffer.text();
        assert_eq!(output.matches("Lean request received").count(), 1);
        assert_eq!(output.matches("Lean response ready").count(), 1);
        assert!(output.contains(r#"kind="invalid""#));
        assert!(output.contains(r#"outcome="tool_error""#));
        assert!(!output.contains("SECRET"));
    }

    #[tokio::test]
    async fn task_negotiation_preserves_sync_payload_and_service_clone_results() {
        let (_guard, buffer) = crate::request_log::tests::capture();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lakefile.lean"), "package status\nlean_lib Status\n").unwrap();
        std::fs::write(dir.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
        std::fs::write(dir.path().join("lake-manifest.json"), "{}\n").unwrap();
        let broker = ProjectBroker::new(crate::broker::BrokerConfig {
            config_default: Some(dir.path().to_path_buf()),
            env_default: None,
            cwd: dir.path().to_path_buf(),
            max_projects: crate::broker::BrokerConfig::default_max_projects(),
            idle_timeout: Duration::ZERO,
        });
        let service = LeanHostService::new(broker, ToolConfig::default());
        // Establish an in-memory legacy peer without enabling rmcp's client
        // feature, opening a socket, or starting a Lean worker.
        let (server_io, mut client_io) = tokio::io::duplex(65_536);
        client_io.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
        let running = service.clone().serve(server_io).await.unwrap();
        let context = RequestContext::new(NumberOrString::Number(2), running.peer().clone());
        let request = CallToolRequestParams::new("lean_verify")
            .with_arguments(serde_json::from_value(serde_json::json!({"targets": []})).unwrap());
        let synchronous = service.call_tool(request.clone(), context.clone()).await.unwrap();
        let CallToolResponse::Complete(synchronous) = synchronous else {
            unreachable!("legacy client must receive a synchronous result")
        };
        let mut task_context = context.clone();
        task_context
            .meta
            .set_client_capabilities(ClientCapabilities::builder().enable_tasks().build());
        let malformed = CallToolRequestParams::new("lean_context")
            .with_arguments(serde_json::from_value(serde_json::json!({"kind": 7})).unwrap());
        let sync_error = service.call_tool(malformed.clone(), context.clone()).await.unwrap();
        let task_error = service.call_tool(malformed, task_context.clone()).await.unwrap();
        let (CallToolResponse::Complete(sync_error), CallToolResponse::Complete(task_error)) = (sync_error, task_error)
        else {
            unreachable!("invalid arguments must be rejected before task acceptance")
        };
        assert_eq!(sync_error.is_error, Some(true));
        assert_eq!(
            serde_json::to_value(sync_error).unwrap(),
            serde_json::to_value(task_error).unwrap()
        );
        let accepted = service.call_tool(request, task_context).await.unwrap();
        let CallToolResponse::Task(accepted) = accepted else {
            unreachable!("negotiated client should receive a task handle")
        };
        let clone = service.clone();
        let task_id = accepted.task.task_id;
        assert!(
            clone
                .get_task(GetTaskParams::new(&task_id), context.clone())
                .await
                .is_ok()
        );
        drop(service);
        let final_task = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = clone
                    .get_task(GetTaskParams::new(&task_id), context.clone())
                    .await
                    .unwrap()
                    .task;
                if task.status().is_terminal() {
                    break task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(final_task.status(), TaskStatus::Completed);
        let TaskPayload::Completed { result } = final_task.payload else {
            unreachable!("task result must be retained")
        };
        assert_eq!(
            serde_json::Value::Object(result),
            serde_json::to_value(synchronous).unwrap()
        );
        clone
            .cancel_task(CancelTaskParams::new(&task_id), context.clone())
            .await
            .unwrap();
        assert_eq!(
            clone
                .get_task(GetTaskParams::new(&task_id), context)
                .await
                .unwrap()
                .task
                .status(),
            TaskStatus::Completed
        );
        let output = buffer.text();
        assert_eq!(output.matches("Lean request received").count(), 2);
        assert_eq!(output.matches("Lean response ready").count(), 2);
        running.cancel().await.unwrap();
        drop(client_io);
    }

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
