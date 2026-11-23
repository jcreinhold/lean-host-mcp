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
    #[cfg(all(unix, feature = "managed-native-store"))]
    managed_store: Option<Arc<parking_lot::Mutex<crate::native_store::store::NativeStore>>>,
}

impl LeanHostService {
    pub fn new(broker: Arc<ProjectBroker>, config: ToolConfig) -> Self {
        let ctx = ToolContext { broker, config };
        Self {
            ctx,
            tool_router: Self::tool_router(),
            tasks: RequestTasks::default(),
            #[cfg(all(unix, feature = "managed-native-store"))]
            managed_store: None,
        }
    }

    /// Enable bounded durable evidence for epoch-bound retained requests.
    /// This never restarts or replays old work. Unfinished dispatched requests
    /// recovered from a previous broker remain explicitly unknown.
    ///
    /// # Errors
    /// Returns an error if the journal is locked, malformed or inaccessible.
    pub fn with_request_journal(mut self, path: &std::path::Path) -> std::io::Result<Self> {
        self.tasks = RequestTasks::open(path)?;
        Ok(self)
    }

    /// Open the explicitly selected native stage store and advertise admission.
    ///
    /// # Errors
    /// Returns an error if the selected private database cannot be opened or created.
    #[cfg(all(unix, feature = "managed-native-store"))]
    pub fn with_managed_native_store(self, path: &std::path::Path) -> std::io::Result<Self> {
        self.with_managed_native_store_identity(path, None)
    }

    /// Open a managed store while enforcing the enrolled UUID on that connection.
    ///
    /// # Errors
    /// Returns an error for an invalid store or a changed enrollment identity.
    #[cfg(all(unix, feature = "managed-native-store"))]
    pub fn with_managed_native_store_identity(
        mut self,
        path: &std::path::Path,
        expected: Option<[u8; 16]>,
    ) -> std::io::Result<Self> {
        use crate::native_store::store::NativeStore;
        use std::os::unix::fs::MetadataExt as _;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("managed store parent missing"))?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if !parent.is_absolute()
            || parent.canonicalize()? != parent
            || !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(std::io::Error::other(
                "managed store requires a canonical private parent",
            ));
        }
        let store = match std::fs::symlink_metadata(path) {
            Ok(file)
                if file.is_file()
                    && file.nlink() == 1
                    && file.uid() == metadata.uid()
                    && file.mode() & 0o777 == 0o600 =>
            {
                NativeStore::open(path)
            }
            Ok(_) => return Err(std::io::Error::other("managed store requires a private regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && expected.is_none() => {
                NativeStore::create_new(path)
            }
            Err(error) => return Err(error),
        }
        .map_err(std::io::Error::other)?;
        if expected.is_some_and(|uuid| store.identity() != uuid) {
            return Err(std::io::Error::other("original launcher store UUID changed"));
        }
        #[cfg(lean_host_mcp_development_warm)]
        let store = {
            let mut store = store;
            let selected = self.ctx.broker.managed_warm();
            if selected {
                if !self.ctx.broker.managed_only()
                    || (self.ctx.broker.prepared_generation().is_none()
                        && !(self
                            .ctx
                            .broker
                            .native_root(&crate::broker::ProjectHint::Default)
                            .is_ok_and(|root| crate::writer_preparation::closure_preparation_configured(&root))))
                {
                    return Err(std::io::Error::other(
                        "warm inspection requires a managed-only service with prepared input",
                    ));
                }
                use crate::toolchain::{Readiness, ToolchainId, WorkerBinary, WorkerSidecar};
                let root = self
                    .ctx
                    .broker
                    .native_root(&crate::broker::ProjectHint::Default)
                    .map_err(std::io::Error::other)?;
                let pin = ToolchainId::from_lake_root(&root).map_err(std::io::Error::other)?;
                let Readiness::Ready { worker, note: None, .. } = WorkerBinary::resolve_ready_for(&pin) else {
                    return Err(std::io::Error::other(
                        "warm selection requires a verified installed worker",
                    ));
                };
                WorkerSidecar::require_warm_worker(&worker.path, &pin).map_err(std::io::Error::other)?;
                store
                    .promote_warm_contract(expected.is_some())
                    .map_err(std::io::Error::other)?;
            } else if store.warm_development_store().map_err(std::io::Error::other)? {
                return Err(std::io::Error::other("warm store requires --managed-warm-inspections"));
            }
            store
        };
        self.managed_store = Some(Arc::new(parking_lot::Mutex::new(store)));
        self.tool_router.merge(Self::managed_tool_router());
        Ok(self)
    }
}

#[cfg(all(unix, feature = "managed-native-store"))]
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ManagedAdmission {
    request: String,
}

/// A plain-tool bridge for clients without the negotiated MCP Tasks extension.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RetainedRequest {
    action: RetainedAction,
    /// Immutable epoch from the prepared ticket; retain it for every retry.
    request_epoch: Option<String>,
    /// Required with `request_epoch` when acknowledging a consumed result.
    submission_sha256: Option<String>,
    /// Server-issued prepared ticket ID, required for submit/get/cancel/acknowledge.
    request_id: Option<String>,
    /// One of `lean_context`, `lean_trial`, `lean_verify`, `lean_lookup` or `lean_status`.
    tool: Option<String>,
    /// The selected semantic tool's ordinary input object.
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RetainedAction {
    Prepare,
    Submit,
    Get,
    Cancel,
    Acknowledge,
}

#[cfg(all(unix, feature = "managed-native-store"))]
#[tool_router(router = managed_tool_router)]
impl LeanHostService {
    #[tool(
        description = "Discover the managed store, admit immutable stage intent, retrieve an admission, cancel it, or acknowledge its durably saved result by ID and digest."
    )]
    fn lean_managed_admission(
        &self,
        Parameters(input): Parameters<ManagedAdmission>,
    ) -> Result<CallToolResult, McpError> {
        let store = self
            .managed_store
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("operation_disabled", None))?;
        let mut store = store
            .try_lock()
            .ok_or_else(|| McpError::internal_error("store_busy", None))?;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let recovery = crate::native_store::store::boundary::decode(input.request.as_bytes())
            .ok()
            .and_then(|command| command.get_identity())
            .and_then(|(key, capability)| crate::managed_prepared::reconcile_lookup(&mut store, key, &capability));
        #[cfg(target_os = "macos")]
        if let Ok(command) = crate::native_store::store::boundary::decode(input.request.as_bytes())
            && let Some(claim) = command.drain_claim()
        {
            let reply = crate::managed_prepared::drain(&mut store, claim)?;
            return Ok(CallToolResult::success(vec![ContentBlock::text(reply.to_string())]));
        }
        #[cfg(lean_host_mcp_development_warm)]
        if let Ok(command) = crate::native_store::store::boundary::decode(input.request.as_bytes())
            && let Some((key, capability, intent)) = command.create_identity()
        {
            crate::managed_prepared::validate_warm_reuse_create(
                &self.ctx.broker,
                &mut store,
                key,
                &capability,
                intent,
            )?;
        }
        let budget = cfg!(any(target_os = "macos", target_os = "linux"))
            .then(|| Duration::from_millis(self.ctx.broker.request_timeout_millis()));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let validate = |bytes: &[u8], after_writer| {
            crate::managed_prepared::validate_create_at_boundary(bytes, after_writer, self.ctx.broker.managed_warm())
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let validate = |_: &[u8], _| Ok(());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let allow_handoff = crate::managed_prepared::validate_writer_handoff;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let allow_handoff = |_: &crate::native_store::store::Snapshot, _: &[u8]| None;
        let reply = store
            .admission_with_handoff(input.request.as_bytes(), budget, validate, allow_handoff)
            .map_err(|reason| McpError::invalid_params(reason, None))?;
        drop(store);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let reply = {
            let mut reply = crate::managed_prepared::advertise(reply, &self.ctx.broker);
            #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
            if reply.get("acceptance_epoch").is_some()
                && self
                    .ctx
                    .broker
                    .native_root(&crate::broker::ProjectHint::Default)
                    .is_ok()
                && let Some(fields) = reply.as_object_mut()
            {
                let operations = fields.entry("operations").or_insert_with(|| serde_json::json!([]));
                if let Some(operations) = operations.as_array_mut() {
                    operations.extend([
                        serde_json::json!(crate::native_reader::admitted::OPERATION),
                        serde_json::json!(crate::native_reader::admitted::AXIOMS),
                        serde_json::json!(crate::native_reader::admitted::MODULE_LINT),
                        serde_json::json!(crate::native_reader::admitted::CHECKPOINT_LINT),
                        serde_json::json!(crate::native_reader::admitted::ROOT_ARCHITECTURE_LINT),
                    ]);
                }
            }
            if let (Some(recovery), Some(fields)) = (recovery, reply.as_object_mut()) {
                fields.insert("recovery".into(), recovery);
            }
            reply
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(reply.to_string())]))
    }
}

#[tool_router]
impl LeanHostService {
    #[tool(
        description = "Prepare, submit, retrieve or cancel a retained Lean request without MCP Tasks negotiation. For new work call action=prepare with tool and arguments; it reserves a five-minute ticket without executing the tool. Save the returned taskId and immutable request binding, then submit the exact tool/arguments with that request_id and request_epoch. Retrying that ticket never runs twice; missing/expired IDs never create work. Capacity returns explicit no-start backpressure. Get/cancel use the same ticket; acknowledge a durably saved result with its epoch and submission_sha256. Execution disposition is separate from the proof verdict. Journal durability is reported by lean_runtime; old work is never replayed after restart."
    )]
    async fn lean_request(&self, Parameters(request): Parameters<RetainedRequest>) -> Result<CallToolResult, McpError> {
        let task = match request.action {
            RetainedAction::Prepare | RetainedAction::Submit => {
                let preparing = matches!(request.action, RetainedAction::Prepare);
                if self.ctx.broker.managed_only() {
                    let reason = "legacy_request_launch_disabled";
                    let message = "managed-only mode requires the original managed admission";
                    return Err(if preparing {
                        crate::tasks::no_start(reason, message)
                    } else {
                        McpError::invalid_params(message, Some(serde_json::json!({"reason": reason})))
                    });
                }
                if !preparing && request.request_id.is_none() {
                    return Err(crate::tasks::no_start(
                        "invalid_submission",
                        "submit requires a prepared request_id",
                    ));
                }
                // A malformed retry says nothing about work already accepted
                // under this ticket. Only pure preparation has no old lifetime.
                let invalid_submission = |message: &str| {
                    if preparing {
                        crate::tasks::no_start("invalid_submission", message)
                    } else {
                        McpError::invalid_params(
                            message.to_owned(),
                            Some(serde_json::json!({"reason":"invalid_ticket_submission",
                                "execution":{"disposition":"unknown","reason":"invalid_ticket_submission"}})),
                        )
                    }
                };
                if preparing && (request.request_id.is_some() || request.request_epoch.is_some()) {
                    return Err(crate::tasks::no_start(
                        "invalid_submission",
                        "prepare does not accept request_id or request_epoch",
                    ));
                }
                if request.submission_sha256.is_some() {
                    return Err(invalid_submission("prepare/submit do not accept submission_sha256"));
                }
                if !preparing && request.arguments.is_none() {
                    return Err(invalid_submission("submit requires the prepared arguments object"));
                }
                let tool = request
                    .tool
                    .ok_or_else(|| invalid_submission("prepare/submit require tool"))?;
                let call =
                    CallToolRequestParams::new(tool.clone()).with_arguments(request.arguments.unwrap_or_default());
                let parsed = TaskCall::parse(&call).map_err(|error| invalid_submission(&error.message))?;
                let digest = Sha256::digest(
                    serde_json::to_vec(&(&tool, &call.arguments))
                        .map_err(|error| invalid_submission(&error.to_string()))?,
                );
                let mut submission_sha256 = String::with_capacity(64);
                for byte in &digest {
                    let _ = write!(submission_sha256, "{byte:02x}");
                }
                let submission = RequestBinding {
                    server_instance_id: self.ctx.broker.instance_id(),
                    request_epoch: request.request_epoch,
                    tool: tool.clone(),
                    submission_sha256,
                    project: call
                        .arguments
                        .as_ref()
                        .and_then(|args| args.get("project"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                };
                if preparing {
                    self.tasks.prepare(submission)?
                } else {
                    let request_id = request.request_id.ok_or_else(|| {
                        crate::tasks::no_start("invalid_submission", "submit requires a prepared request_id")
                    })?;
                    let service = self.clone();
                    let accepted = self.tasks.submit(&tool, request_id, submission, move || async move {
                        service.run_task(parsed).await
                    })?;
                    self.tasks.get_retained(&accepted.task_id)?
                }
            }
            RetainedAction::Get | RetainedAction::Cancel | RetainedAction::Acknowledge => {
                if request.tool.is_some() || request.arguments.is_some() {
                    return Err(McpError::invalid_params("get/cancel accepts only request_id", None));
                }
                let request_id = request
                    .request_id
                    .ok_or_else(|| McpError::invalid_params("get/cancel requires request_id", None))?;
                if matches!(request.action, RetainedAction::Acknowledge) {
                    let epoch = request
                        .request_epoch
                        .as_deref()
                        .ok_or_else(|| McpError::invalid_params("acknowledge requires request_epoch", None))?;
                    let digest = request
                        .submission_sha256
                        .as_deref()
                        .ok_or_else(|| McpError::invalid_params("acknowledge requires submission_sha256", None))?;
                    match self.tasks.acknowledge_if_retained(&request_id, epoch, digest)? {
                        Some(task) => task,
                        None => {
                            return Ok(carry(
                                &serde_json::json!({
                                    "status":"absent", "acknowledged":true, "result_available":false,
                                    "request_id":request_id, "request_epoch":epoch, "submission_sha256":digest,
                                    "identity_verified":false,
                                }),
                                self.ctx.config.carrier,
                            ));
                        }
                    }
                } else {
                    if request.submission_sha256.is_some() {
                        return Err(McpError::invalid_params(
                            "get/cancel does not accept submission_sha256",
                            None,
                        ));
                    }
                    if matches!(request.action, RetainedAction::Cancel) {
                        self.tasks
                            .cancel_scoped(&request_id, request.request_epoch.as_deref())?;
                    }
                    self.tasks
                        .get_retained_scoped(&request_id, request.request_epoch.as_deref())?
                }
            }
        };
        let value = serde_json::to_value(task).map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(carry(&value, self.ctx.config.carrier))
    }

    #[tool(
        description = "Describe compiled native requirements without work using mode=\"pure_v1\", or verify an actual loaded worker using the default mode=\"loaded\". Pure discovery never starts a worker or preparer."
    )]
    async fn lean_compatibility(
        &self,
        Parameters(request): Parameters<crate::runtime_compatibility::CompatibilityRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if request.project.trim().is_empty() {
            return Err(McpError::invalid_params("explicit project required", None));
        }
        let hint = crate::broker::ProjectHint::Explicit(std::path::PathBuf::from(request.project));
        if matches!(request.mode, crate::runtime_compatibility::CompatibilityMode::PureV1) {
            let result = self
                .ctx
                .broker
                .describe_native_compatibility(&hint)
                .map_err(McpError::from)?;
            return Ok(carry(&result, self.ctx.config.carrier));
        }
        let result = self
            .ctx
            .broker
            .native_compatibility(hint)
            .await
            .map_err(McpError::from)?;
        Ok(carry(&result, self.ctx.config.carrier))
    }

    #[tool(
        description = "Observe worker activity or atomically lease the idle worker pool. Actions: status, acquire, renew, release. Leases pause MCP admission; independent artifact writers may retire idle caches. Set retire_idle for explicit retirement and renew before expiry while relying on the pause."
    )]
    async fn lean_runtime(
        &self,
        Parameters(request): Parameters<RuntimeControlRequest>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mut result = self.ctx.broker.runtime_control(&request);
        result.effective_settings.server = Some(self.ctx.config.into());
        result.request_registry = Some(self.tasks.status());
        result.capabilities.push("retained_request_lifecycle_v3".to_owned());
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
        #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
        if request.name.as_ref() == "lean_managed_admission"
            && let Some(store) = &self.managed_store
            && let Some(arguments) = &request.arguments
            && let Ok(input) = serde_json::from_value::<ManagedAdmission>(serde_json::Value::Object(arguments.clone()))
            && let Ok(command) = crate::native_store::store::boundary::decode(input.request.as_bytes())
            && let Some((key, capability)) = command.start_identity()
        {
            #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
            if let Some(result) =
                crate::native_reader::admitted::start(&self.ctx.broker, store, key, capability.clone()).await?
            {
                return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(result.to_string()),
                ])));
            }
            let result = crate::managed_prepared::start(&self.ctx.broker, store, key, capability).await?;
            return Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(result.to_string()),
            ])));
        }
        // Coordination is immediate and must return its lease token directly,
        // even for clients that negotiate asynchronous Lean job handles.
        if !self.ctx.broker.managed_only()
            && !matches!(
                request.name.as_ref(),
                "lean_runtime" | "lean_request" | "lean_compatibility"
            )
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
        let capabilities = ServerCapabilities::builder().enable_tools();
        info.capabilities = if self.ctx.broker.managed_only() {
            capabilities.build()
        } else {
            capabilities.enable_tasks().build()
        };
        info.server_info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_website_url("https://github.com/jcreinhold/lean-host-mcp");
        let common = "MCP server hosting Lean 4 in supervised worker children via lean-rs. \
             Tools expose five semantic Lean job families: context, trial, verification, lookup, and status. \
             Context, trial, lookup, and status select a mode with `kind`; verification takes explicit target groups.";
        let execution = if self.ctx.broker.managed_only() {
            " Use lean_managed_admission for new jobs. Historical lean_request tickets support retrieval, cancellation, and acknowledgement."
        } else {
            " Clients advertising io.modelcontextprotocol/tasks may receive task handles; results remain available for ten minutes \
             after completion while this server process lives. Use lean_request to submit, retrieve or cancel retained requests \
             without task negotiation."
        };
        info.instructions = Some(format!("{common}{execution}"));
        info
    }
}

impl LeanHostService {
    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    fn managed_setup_call(
        &self,
        original: Option<serde_json::Value>,
        required: Option<&'static str>,
    ) -> Result<Option<crate::writer_setup_verification::ManagedCall<'_>>, McpError> {
        let Some(required) = required else {
            if original.is_some() {
                return Err(McpError::invalid_params("managed_original_not_applicable", None));
            }
            return Ok(None);
        };
        if !self.ctx.broker.managed_only() {
            if original.is_some() {
                return Err(McpError::invalid_params("managed_only_required", None));
            }
            return Ok(None);
        }
        let store = self
            .managed_store
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("managed_store_required", None))?;
        let reference = original
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| McpError::invalid_params(required, None))?;
        Ok(Some(crate::writer_setup_verification::ManagedCall { reference, store }))
    }

    async fn run_task(&self, call: TaskCall) -> Result<CallToolResult, McpError> {
        let mut log = call.request_log();
        let result = match call {
            TaskCall::Context(req) => {
                let req = req.into_inner();
                #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
                {
                    let mut req = req;
                    let position = req.kind().is_some_and(|kind| {
                        tools::request_aliases::canonical_kind("lean_context", kind) == "proof_position"
                    });
                    let original = req.args.remove("managed_original");
                    let managed = self
                        .managed_setup_call(original, position.then_some("managed_proof_position_original_required"))?;
                    if managed.is_some() {
                        tools::semantic::lean_context_inner(&self.ctx, req, managed).await
                    } else {
                        tools::semantic::lean_context(&self.ctx, req).await
                    }
                }
                #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
                tools::semantic::lean_context(&self.ctx, req).await
            }
            TaskCall::Trial(req) => {
                let req = req.into_inner();
                #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
                {
                    let mut req = req;
                    let candidate = req.kind().is_some_and(|kind| {
                        matches!(
                            tools::request_aliases::canonical_kind("lean_trial", kind),
                            "command" | "proof_step"
                        )
                    });
                    let original = req.args.remove("managed_original");
                    let managed =
                        self.managed_setup_call(original, candidate.then_some("managed_trial_original_required"))?;
                    tools::semantic::lean_trial_inner(&self.ctx, req, managed).await
                }
                #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
                tools::semantic::lean_trial(&self.ctx, req).await
            }
            TaskCall::Verify(req) => {
                #[allow(
                    unused_mut,
                    reason = "managed builds remove the original reference before semantic decoding"
                )]
                let mut args = req.into_inner();
                #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
                {
                    let original = args
                        .as_object_mut()
                        .and_then(|object| object.remove("managed_original"));
                    let managed = self.managed_setup_call(original, Some("managed_verify_original_required"))?;
                    tools::semantic::lean_verify_raw_inner(&self.ctx, args, managed).await
                }
                #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
                tools::semantic::lean_verify_raw(&self.ctx, args).await
            }
            TaskCall::Lookup(req) => {
                let req = req.into_inner();
                #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
                {
                    let mut req = req;
                    let required =
                        req.kind().and_then(
                            |kind| match tools::request_aliases::canonical_kind("lean_lookup", kind) {
                                "declarations" => Some("managed_outline_original_required"),
                                "declaration" => Some("managed_declaration_original_required"),
                                "proof_search" => Some("managed_proof_search_original_required"),
                                _ => None,
                            },
                        );
                    let original = req.args.remove("managed_original");
                    let managed = self.managed_setup_call(original, required)?;
                    tools::semantic::lean_lookup_inner(&self.ctx, req, managed).await
                }
                #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
                tools::semantic::lean_lookup(&self.ctx, req).await
            }
            TaskCall::Status(req) => {
                #[allow(
                    unused_mut,
                    reason = "managed builds remove the original reference before semantic decoding"
                )]
                let mut req = req.into_inner();
                #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
                {
                    let mut req = req;
                    let diagnostics =
                        tools::request_aliases::canonical_kind("lean_status", req.kind().unwrap_or("project"))
                            == "file_diagnostics";
                    let original = req.args.remove("managed_original");
                    let managed = self
                        .managed_setup_call(original, diagnostics.then_some("managed_diagnostics_original_required"))?;
                    tools::semantic::lean_status_inner(&self.ctx, req, managed).await
                }
                #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
                tools::semantic::lean_status(&self.ctx, req).await
            }
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

    #[cfg(all(unix, feature = "managed-native-store"))]
    #[tokio::test]
    async fn managed_admission_recovers_original_and_keeps_stock_tool_table() {
        use serde_json::{Value, json};
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().canonicalize().unwrap().join("stages.sqlite3");
        let stock = || {
            LeanHostService::new(
                ProjectBroker::new(crate::broker::BrokerConfig {
                    cwd: dir.path().to_owned(),
                    config_default: None,
                    env_default: None,
                    max_projects: crate::broker::BrokerConfig::default_max_projects(),
                    max_workers_per_project: std::num::NonZeroUsize::MIN,
                    idle_timeout: Duration::ZERO,
                }),
                ToolConfig::default(),
            )
        };
        let call = |service: &LeanHostService, command: Value| {
            service
                .lean_managed_admission(Parameters(ManagedAdmission {
                    request: json!({"version":1, "command":command}).to_string(),
                }))
                .map(|reply| {
                    serde_json::from_str::<Value>(&reply.content.first().unwrap().as_text().unwrap().text).unwrap()
                })
        };
        assert!(stock().get_tool("lean_managed_admission").is_none());
        assert!(!path.exists());
        let service = stock().with_managed_native_store(&path).unwrap();
        assert!(service.get_tool("lean_managed_admission").is_some());
        let discovery = call(&service, json!({"op":"discover"})).unwrap();
        let key =
            json!({"store":discovery["store"], "epoch":discovery["acceptance_epoch"], "nonce":vec![9;16], "stage":1});
        let cap = vec![1; 32];
        let create = |intent| json!({"op":"create","key":key,"capability":cap,"intent":intent});
        let reserved =
            json!({"store":discovery["store"],"epoch":discovery["acceptance_epoch"],"nonce":vec![6;16],"stage":1});
        let with_drain = |secret| {
            json!({
                "op":"create","key":reserved,"capability":cap,"intent":[42],"drain_authorization":secret
            })
        };
        assert_eq!(
            call(&service, with_drain(json!(cap))).unwrap_err().message,
            "capability_denied"
        );
        let admitted = call(&service, with_drain(json!(vec![3; 32]))).unwrap();
        assert!(admitted.get("drain_authorization").is_none());
        assert_eq!(
            call(&service, with_drain(json!(vec![4; 32]))).unwrap_err().message,
            "capability_denied",
        );
        let original = call(&service, create(vec![42])).unwrap();
        assert_eq!(original["dispatch_enabled"], false);
        drop(service);
        let service = stock().with_managed_native_store(&path).unwrap();
        assert_eq!(call(&service, create(vec![42])).unwrap(), original);
        {
            let store = service.managed_store.as_ref().unwrap().lock();
            let original_key = serde_json::from_value(reserved).unwrap();
            assert!(store.authorize_drain(original_key, None).is_err());
            assert!(store.authorize_drain(original_key, Some([1; 32])).is_err());
            assert!(store.authorize_drain(original_key, Some([3; 32])).is_ok());
        }
        assert_eq!(
            call(&service, json!({"op":"get","key":key,"capability":cap})).unwrap(),
            original
        );
        assert_eq!(call(&service, create(vec![43])).unwrap_err().message, "intent_changed");
        assert_eq!(
            call(
                &service,
                json!({"op":"create","key":key,"capability":vec![2;32],"intent":[42]})
            )
            .unwrap_err()
            .message,
            "capability_denied"
        );
        assert_eq!(
            call(&service, json!({"op":"start","key":key,"capability":cap}))
                .unwrap_err()
                .message,
            "operation_disabled"
        );
        assert!(stock().get_tool("lean_managed_admission").is_none());

        // Use an ordinary in-memory peer for the real tool router, without
        // keeping a clone of the admitted store open across restart.
        let (server_io, mut client_io) = tokio::io::duplex(65_536);
        client_io.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
        let peer = stock().serve(server_io).await.unwrap();
        let context = RequestContext::new(NumberOrString::Number(2), peer.peer().clone());
        let routed = |command: Value| {
            CallToolRequestParams::new("lean_managed_admission").with_arguments(
                serde_json::from_value(json!({"request":json!({"version":1,"command":command}).to_string()})).unwrap(),
            )
        };
        let cancel = |capability: Vec<u8>| routed(json!({"op":"cancel","key":key,"capability":capability}));
        assert_eq!(
            service
                .call_tool(cancel(vec![2; 32]), context.clone())
                .await
                .unwrap_err()
                .message,
            "capability_denied"
        );
        let mut other_revision = key.clone();
        other_revision["nonce"] = json!(vec![8; 16]);
        assert_eq!(
            service
                .call_tool(
                    routed(json!({"op":"cancel","key":other_revision,"capability":cap})),
                    context.clone()
                )
                .await
                .unwrap_err()
                .message,
            "unknown_stage"
        );
        assert_eq!(
            call(&service, json!({"op":"get","key":key,"capability":cap})).unwrap(),
            original
        );
        // The commit succeeds, but the caller never observes the cancel reply.
        drop(service.call_tool(cancel(cap.clone()), context.clone()).await.unwrap());
        drop(service);
        let service = stock().with_managed_native_store(&path).unwrap();
        let saved = call(&service, json!({"op":"get","key":key,"capability":cap})).unwrap();
        assert_eq!(saved["execution"], "not_started");
        assert_eq!(saved["cancel_requested"], true);
        assert_eq!(saved["key"], key);
        assert_eq!(call(&service, create(vec![42])).unwrap(), saved);
        let CallToolResponse::Complete(repeat) = service.call_tool(cancel(cap.clone()), context.clone()).await.unwrap()
        else {
            unreachable!("cancel must return a synchronous reply");
        };
        assert_eq!(
            serde_json::from_str::<Value>(&repeat.content.first().unwrap().as_text().unwrap().text).unwrap(),
            saved
        );

        // Seed a legacy untimed consumed revision through the private store
        // fixture. Public cancellation still must not certify no start.
        {
            use crate::native_store::store::{Capability, Consume, Scope};
            let mut store = service.managed_store.as_ref().unwrap().lock();
            let fixture = json!({"version":1,"command":{
                "op":"create","key":other_revision,"capability":cap,"intent":[42]
            }});
            store.admission(fixture.to_string().as_bytes()).unwrap();
            let native_key = serde_json::from_value(other_revision.clone()).unwrap();
            let cap = Capability([1; 32]);
            let binding = store
                .bind_scope(
                    native_key,
                    &cap,
                    Scope {
                        id: [3; 16],
                        generation: 1,
                        owner: [4; 16],
                    },
                )
                .unwrap();
            assert!(matches!(
                store.consume_launch(binding, &cap).unwrap(),
                Consume::Granted(_)
            ));
        }
        let CallToolResponse::Complete(consumed) = service
            .call_tool(
                routed(json!({"op":"cancel","key":other_revision,"capability":cap})),
                context,
            )
            .await
            .unwrap()
        else {
            unreachable!("cancel must return a synchronous reply");
        };
        let consumed: Value = serde_json::from_str(&consumed.content.first().unwrap().as_text().unwrap().text).unwrap();
        assert_eq!(consumed["execution"], "unknown");
        assert_eq!(consumed["cancel_requested"], true);
        assert_eq!(consumed["result"]["state"], "pending");

        // A stored reply may outlive both the caller connection and the
        // broker, even when the exact worker's retirement is unconfirmed.
        let saved_key = consumed["key"].clone();
        let body = b"checked reply bytes";
        {
            use crate::native_store::store::{Capability, ResourceReport, Scope};
            let mut store = service.managed_store.as_ref().unwrap().lock();
            let key = serde_json::from_value(saved_key.clone()).unwrap();
            let cap = Capability([1; 32]);
            let binding = crate::native_store::store::Binding {
                key,
                scope: Scope {
                    id: [3; 16],
                    generation: 1,
                    owner: [4; 16],
                },
            };
            store.observe_resource(binding, &cap, ResourceReport::Open).unwrap();
            store.observe_result(binding, &cap, [5; 16], body).unwrap();
        }
        drop(service);
        let service = stock().with_managed_native_store(&path).unwrap();
        let saved = call(&service, json!({"op":"get","key":saved_key,"capability":cap})).unwrap();
        assert_eq!(saved["result"]["state"], "retained");
        assert_eq!(saved["result"]["id"], json!(vec![5; 16]));
        assert_eq!(saved["result"]["sha256"], json!(sha2::Sha256::digest(body).to_vec()));
        assert_eq!(saved["result"]["body"], json!(body));
        assert_eq!(saved["custody"], "open");
        assert_eq!(
            call(&service, json!({"op":"get","key":saved_key,"capability":vec![2;32]}))
                .unwrap_err()
                .message,
            "capability_denied"
        );
        assert_eq!(
            call(&service, json!({"op":"start","key":saved_key,"capability":cap}))
                .unwrap_err()
                .message,
            "operation_disabled"
        );
        let ack = |capability, digest| {
            json!({
                "op":"ack", "key":saved_key, "capability":capability,
                "result_id":saved["result"]["id"], "result_digest":digest,
            })
        };
        let digest = saved["result"]["sha256"].clone();
        assert_eq!(
            call(&service, ack(json!(vec![2; 32]), digest.clone()))
                .unwrap_err()
                .message,
            "capability_denied"
        );
        assert_eq!(
            call(&service, ack(json!(cap), json!(vec![2; 32]))).unwrap_err().message,
            "result_changed"
        );
        assert_eq!(
            call(&service, json!({"op":"get","key":saved_key,"capability":cap})).unwrap(),
            saved
        );
        let acknowledged = {
            use crate::native_store::store::{CallerJournal, CallerKey, Capability};
            use tokio::io::{AsyncBufReadExt as _, BufReader, DuplexStream};

            async fn wire(stream: &mut BufReader<DuplexStream>, message: Value) -> Value {
                stream
                    .get_mut()
                    .write_all(format!("{message}\n").as_bytes())
                    .await
                    .unwrap();
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                serde_json::from_str(&line).unwrap()
            }
            async fn wire_call(stream: &mut BufReader<DuplexStream>, id: u64, command: Value) -> Value {
                let reply = wire(
                    stream,
                    json!({
                        "jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                            "name":"lean_managed_admission",
                            "arguments":{"request":json!({"version":1,"command":command}).to_string()}
                        }
                    }),
                )
                .await;
                assert_eq!(reply["id"], id);
                assert!(reply.get("error").is_none(), "{reply}");
                serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
            }
            let (server_io, client_io) = tokio::io::duplex(65_536);
            let mut client = BufReader::new(client_io);
            let app = service.clone();
            let listener = tokio::spawn(async move { app.serve(server_io).await.unwrap() });
            assert!(
                wire(
                    &mut client,
                    json!({
                        "jsonrpc":"2.0","id":1,"method":"initialize","params":{
                            "protocolVersion":"2025-11-25","capabilities":{},
                            "clientInfo":{"name":"caller-save-regression","version":"1"}
                        }
                    })
                )
                .await
                .get("result")
                .is_some()
            );
            client
                .get_mut()
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await
                .unwrap();
            let running = listener.await.unwrap();

            let caller_path = path.with_file_name("caller.sqlite3");
            let mut journal = CallerJournal::create_new(&caller_path).unwrap();
            let native_key = serde_json::from_value(saved_key.clone()).unwrap();
            let request = journal
                .bind(
                    CallerKey {
                        job: vec![7],
                        stage: 1,
                        revision: 1,
                    },
                    &[42],
                    native_key,
                    Capability([1; 32]),
                )
                .unwrap();
            let source = service
                .managed_store
                .as_ref()
                .unwrap()
                .lock()
                .get(native_key)
                .unwrap()
                .unwrap();
            let get = json!({"op":"get","key":saved_key,"capability":cap});
            let received = wire_call(&mut client, 2, get.clone()).await;
            assert_eq!(received, saved);
            let fault = rusqlite::Connection::open(&caller_path).unwrap();
            fault
                .execute_batch(
                    "CREATE TRIGGER reject_caller_result BEFORE INSERT ON accepted_results
                 BEGIN SELECT RAISE(ABORT,'caller result insert rejected'); END;",
                )
                .unwrap();
            let error = journal.save_result(&request, &source).unwrap_err();
            assert!(error.to_string().contains("caller result insert rejected"), "{error}");
            assert!(journal.saved_result(&request.caller).unwrap().is_none());
            assert_eq!(wire_call(&mut client, 3, get).await, received);

            fault.execute_batch("DROP TRIGGER reject_caller_result").unwrap();
            let accepted = journal.save_result(&request, &source).unwrap();
            let cold = CallerJournal::open(&caller_path).unwrap();
            assert_eq!(cold.saved_result(&request.caller).unwrap(), Some(accepted.clone()));
            assert_eq!(json!(accepted.result_id()), received["result"]["id"]);
            assert_eq!(json!(accepted.hash()), received["result"]["sha256"]);
            assert_eq!(json!(accepted.body()), received["result"]["body"]);
            let command = ack(json!(cap), json!(accepted.hash()));
            let reply = wire_call(&mut client, 4, command.clone()).await;
            assert_eq!(wire_call(&mut client, 5, command).await, reply);
            running.cancel().await.unwrap();
            reply
        };
        assert_eq!(acknowledged["result"]["state"], "acknowledged");
        assert_eq!(acknowledged["result"]["body"], json!(body));
        assert_eq!(acknowledged["custody"], "open");
        assert_eq!(acknowledged["intent"], json!([42]));
        {
            use crate::native_store::store::{Capability, ResourceReport};
            let mut store = service.managed_store.as_ref().unwrap().lock();
            let key = serde_json::from_value(saved_key.clone()).unwrap();
            let binding = crate::native_store::store::Binding {
                key,
                scope: store.get(key).unwrap().unwrap().scope.unwrap(),
            };
            store
                .observe_resource(binding, &Capability([1; 32]), ResourceReport::Closed)
                .unwrap();
        }
        drop(service);
        let service = stock().with_managed_native_store(&path).unwrap();
        let expired = call(&service, ack(json!(cap), digest.clone())).unwrap();
        assert_eq!(expired["execution"], "unknown");
        assert_eq!(expired["custody"], "closed");
        assert!(expired["intent"].is_null());
        assert_eq!(expired["intent_sha256"], json!(sha2::Sha256::digest([42]).to_vec()));
        let raw = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            raw.query_row(
                "SELECT length(intent),length(compacted_intent_sha256),result_body IS NULL
                 FROM stages WHERE nonce=?1",
                [[8u8; 16]],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, bool>(2)?)),
            )
            .unwrap(),
            (0, 32, true)
        );
        drop(raw);
        let equal_create = json!({"op":"create","key":saved_key,"capability":cap,"intent":[42]});
        assert_eq!(call(&service, equal_create.clone()).unwrap(), expired);
        let mut changed = equal_create;
        changed["intent"] = json!([43]);
        assert_eq!(call(&service, changed).unwrap_err().message, "intent_changed");
        assert_eq!(
            expired["result"],
            json!({"state":"expired","id":saved["result"]["id"],"sha256":digest})
        );
        assert_eq!(
            call(&service, json!({"op":"get","key":saved_key,"capability":cap})).unwrap(),
            expired
        );

        // Once the operator is offline, preserve the refused bytes and choose a
        // fresh explicit path. A fresh store does not accept the old ID.
        drop(service);
        let mut damaged = std::fs::read(&path).unwrap();
        damaged[0] ^= 0xff;
        std::fs::write(&path, &damaged).unwrap();
        assert!(stock().with_managed_native_store(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        let quarantine = path.with_extension("quarantine");
        std::fs::rename(&path, &quarantine).unwrap();
        let replacement = stock()
            .with_managed_native_store(&path.with_file_name("replacement.sqlite3"))
            .unwrap();
        assert_ne!(
            call(&replacement, json!({"op":"discover"})).unwrap()["store"],
            discovery["store"]
        );
        assert_eq!(
            call(&replacement, json!({"op":"get","key":saved_key,"capability":cap}))
                .unwrap_err()
                .message,
            "wrong_store_or_epoch"
        );
        assert_eq!(std::fs::read(quarantine).unwrap(), damaged);
        let fresh = call(&replacement, json!({"op":"discover"})).unwrap();
        let replacement_path = path.with_file_name("replacement.sqlite3");
        let kept = json!({"store":fresh["store"],"epoch":fresh["acceptance_epoch"],"nonce":vec![7;16],"stage":1});
        let kept_create = json!({"op":"create","key":kept,"capability":cap,"intent":[42]});
        let kept_status = call(&replacement, kept_create.clone()).unwrap();
        let seed = rusqlite::Connection::open(replacement_path).unwrap();
        seed.execute_batch(
            "WITH RECURSIVE ids(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM ids WHERE i<1023)
             INSERT INTO stages(epoch,nonce,stage,intent,cap_hash)
             SELECT 1,CAST(printf('%016d',i) AS BLOB),1,x'2a',zeroblob(32) FROM ids",
        )
        .unwrap();
        let mut excess = kept_create.clone();
        excess["key"]["nonce"] = json!(vec![8; 16]);
        assert_eq!(call(&replacement, excess.clone()).unwrap_err().message, "store_full");
        let seeded_key = |id| {
            json!({
                "store":fresh["store"],"epoch":fresh["acceptance_epoch"],
                "nonce":format!("{id:016}").as_bytes(),"stage":1,
            })
        };
        let spent = seeded_key(1);
        let still_open = seeded_key(2);
        let unknown = seeded_key(3);
        let unacknowledged = seeded_key(4);
        let proof = [5u8; 16];
        let hash = [6u8; 32];
        for (id, report, result, ack) in [(1, 3, 3, 0), (2, 1, 3, 1), (3, 0, 3, 1), (4, 3, 3, 0)] {
            seed.execute(
                "UPDATE stages SET gate=1,resource_report=?1,result_kind=?2,managed_ack=?3,
                 result_id=?4,result_hash=?5,cap_hash=(SELECT cap_hash FROM stages WHERE nonce=?6),
                 scope=?7,generation=1,owner=x'01010101010101010101010101010101'
                 WHERE nonce=?7",
                rusqlite::params![
                    report,
                    result,
                    ack,
                    proof,
                    hash,
                    [7u8; 16],
                    format!("{id:016}").as_bytes()
                ],
            )
            .unwrap();
        }
        assert_eq!(
            call(
                &replacement,
                json!({"op":"ack","key":spent,"capability":cap,"result_id":proof,"result_digest":hash})
            )
            .unwrap()["result"]["state"],
            "expired"
        );
        let next = call(&replacement, json!({"op":"discover"})).unwrap()["acceptance_epoch"].clone();
        assert_eq!(next, 2);
        assert_eq!(
            call(&replacement, json!({"op":"start","key":spent,"capability":cap}))
                .unwrap_err()
                .message,
            "unknown_stage"
        );
        assert_eq!(
            call(
                &replacement,
                json!({"op":"create","key":spent,"capability":cap,"intent":[42]})
            )
            .unwrap_err()
            .message,
            "wrong_store_or_epoch"
        );
        for (key, expected) in [
            (still_open, "open"),
            (unknown, "unobserved"),
            (unacknowledged, "closed"),
        ] {
            assert_eq!(
                call(&replacement, json!({"op":"get","key":key,"capability":cap})).unwrap()["custody"],
                expected
            );
        }
        excess["key"]["epoch"] = next;
        assert_eq!(call(&replacement, excess).unwrap()["execution"], "admitted");
        assert_eq!(call(&replacement, kept_create).unwrap(), kept_status);
        assert_eq!(
            call(&replacement, json!({"op":"cancel","key":kept,"capability":cap})).unwrap()["execution"],
            "not_started"
        );
        peer.cancel().await.unwrap();
    }

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
                max_workers_per_project: std::num::NonZeroUsize::MIN,
                idle_timeout: Duration::ZERO,
            }),
            ToolConfig::default(),
        );
        let prepare = RetainedRequest {
            action: RetainedAction::Prepare,
            request_epoch: None,
            submission_sha256: None,
            request_id: None,
            tool: Some("lean_trial".to_owned()),
            arguments: Some(
                serde_json::from_value(serde_json::json!({"kind":"SECRET_KIND","commands":"SECRET_PROOF"})).unwrap(),
            ),
        };
        let prepared = service.lean_request(Parameters(prepare)).await.unwrap();
        let prepared: serde_json::Value =
            serde_json::from_str(&prepared.content.first().unwrap().as_text().unwrap().text).unwrap();
        let request_id = prepared
            .get("taskId")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_owned();
        let epoch = prepared
            .get("request")
            .and_then(|binding| binding.get("request_epoch"))
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_owned();
        assert_eq!(buffer.text().matches("Lean request received").count(), 0);
        let submit = || RetainedRequest {
            action: RetainedAction::Submit,
            request_epoch: Some(epoch.clone()),
            submission_sha256: None,
            request_id: Some(request_id.clone()),
            tool: Some("lean_trial".to_owned()),
            arguments: Some(
                serde_json::from_value(serde_json::json!({"kind":"SECRET_KIND","commands":"SECRET_PROOF"})).unwrap(),
            ),
        };
        service.lean_request(Parameters(submit())).await.unwrap();
        service.lean_request(Parameters(submit())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !service.tasks.get(&request_id).unwrap().status().is_terminal() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for _ in 0..3 {
            service
                .lean_request(Parameters(RetainedRequest {
                    action: RetainedAction::Get,
                    request_epoch: Some(epoch.clone()),
                    submission_sha256: None,
                    request_id: Some(request_id.clone()),
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

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    #[tokio::test]
    async fn managed_only_refuses_legacy_launches_and_keeps_historical_tickets() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let service = |managed| {
            LeanHostService::new(
                ProjectBroker::new_with_runtime_config(
                    crate::broker::BrokerConfig {
                        config_default: None,
                        env_default: None,
                        cwd: dir.path().to_owned(),
                        max_projects: std::num::NonZeroUsize::MIN,
                        max_workers_per_project: std::num::NonZeroUsize::MIN,
                        idle_timeout: Duration::ZERO,
                    },
                    crate::project::ProjectRuntimeConfig::default().with_managed_only(managed),
                ),
                ToolConfig::default(),
            )
        };
        let ordinary = service(false);
        let mut managed = service(true)
            .with_managed_native_store(&dir.path().canonicalize().unwrap().join("stages.sqlite3"))
            .unwrap();
        let (server_io, mut client_io) = tokio::io::duplex(65_536);
        client_io.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
        let running = ordinary.clone().serve(server_io).await.unwrap();
        let mut context = RequestContext::new(NumberOrString::Number(2), running.peer().clone());
        context
            .meta
            .set_client_capabilities(ClientCapabilities::builder().enable_tasks().build());
        let call = CallToolRequestParams::new("lean_verify")
            .with_arguments(serde_json::from_value(serde_json::json!({"targets": []})).unwrap());
        let error = managed.call_tool(call.clone(), context.clone()).await.unwrap_err();
        assert_eq!(error.message, "managed_verify_original_required");
        assert!(!managed.get_info().capabilities.supports_tasks());
        assert_eq!(managed.tasks.status().active, 0);
        assert_eq!(managed.tasks.status().retained_terminal, 0);
        assert!(matches!(
            ordinary.call_tool(call, context).await.unwrap(),
            CallToolResponse::Task(_)
        ));

        let request = |action, id, epoch, digest| {
            Parameters(RetainedRequest {
                action,
                request_id: id,
                request_epoch: epoch,
                submission_sha256: digest,
                tool: matches!(action, RetainedAction::Prepare | RetainedAction::Submit)
                    .then(|| "lean_trial".to_owned()),
                arguments: matches!(action, RetainedAction::Prepare | RetainedAction::Submit)
                    .then(|| serde_json::from_value(serde_json::json!({"kind": "invalid"})).unwrap()),
            })
        };
        let err = managed
            .lean_request(request(RetainedAction::Prepare, None, None, None))
            .await
            .unwrap_err();
        assert_eq!(err.data.unwrap()["reason"], "legacy_request_launch_disabled");
        assert_eq!(managed.tasks.status().active, 0);
        assert_eq!(managed.tasks.status().retained_terminal, 0);

        let ticket = ordinary
            .lean_request(request(RetainedAction::Prepare, None, None, None))
            .await
            .unwrap();
        let ticket: serde_json::Value =
            serde_json::from_str(&ticket.content.first().unwrap().as_text().unwrap().text).unwrap();
        let id = ticket["taskId"].as_str().unwrap().to_owned();
        let epoch = ticket["request"]["request_epoch"].as_str().unwrap().to_owned();
        let digest = ticket["request"]["submission_sha256"].as_str().unwrap().to_owned();
        managed.tasks = ordinary.tasks.clone();
        let original = || (Some(id.clone()), Some(epoch.clone()));
        let (key, epoch_arg) = original();
        let err = managed
            .lean_request(request(RetainedAction::Submit, key, epoch_arg, None))
            .await
            .unwrap_err();
        assert_eq!(err.data.unwrap()["reason"], "legacy_request_launch_disabled");
        assert!(err.message.contains("original managed admission"));
        let (key, epoch_arg) = original();
        let old = managed
            .lean_request(request(RetainedAction::Get, key, epoch_arg, None))
            .await
            .unwrap();
        let old: serde_json::Value =
            serde_json::from_str(&old.content.first().unwrap().as_text().unwrap().text).unwrap();
        assert_eq!(old["execution"]["reason"], "prepared");

        let (key, epoch_arg) = original();
        managed
            .lean_request(request(RetainedAction::Cancel, key, epoch_arg, None))
            .await
            .unwrap();
        let (key, epoch_arg) = original();
        managed
            .lean_request(request(RetainedAction::Get, key, epoch_arg, None))
            .await
            .unwrap();
        let (key, epoch_arg) = original();
        managed
            .lean_request(request(RetainedAction::Acknowledge, key, epoch_arg, Some(digest)))
            .await
            .unwrap();
        running.cancel().await.unwrap();
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
            max_workers_per_project: std::num::NonZeroUsize::MIN,
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
            worker_stop_receipt: None,
            preparation_closed: false,
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
