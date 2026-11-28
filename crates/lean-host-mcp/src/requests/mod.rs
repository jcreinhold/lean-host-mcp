//! Requests a client names with an id: one row each, and one launch.
//!
//! A client sends `request_id` with a tool call, or calls `lean_run`. The
//! server records the call in the request table before anything runs, runs it
//! at most once, and keeps its result, so the same id returns the same result
//! after a dropped connection, from another client, or after a restart. An id
//! matches `^[A-Za-z0-9._:-]{8,128}$`. Ids that start with `srv-` belong to the
//! server, which gives one to each call that comes without an id; such a call
//! gets no row.
//!
//! A build takes the project's write turn and stops the project's workers; a
//! check takes the read turn. Then the row says `running`, and only then does
//! work start.
//! A request ends when it finishes, at its deadline, or in recovery at the next
//! start (see `recover`). Nothing cancels it, so a dropped connection ends only
//! the client's wait.

mod recover;
pub mod store;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock, ErrorCode, ResultType};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::broker::ProjectHint;
use crate::build::{BuildState, OnRunning};
use crate::cancellation::{CallExecutionEvidence, with_cancellation, with_cancellation_evidence, with_deadline};
use crate::config_file::RequestsFileConfig;
use crate::error::ServerError;
use crate::server::{ToolCall, ToolCallError, carry, respond_semantic};
use crate::tools::{ResponseCarrier, ToolContext};
pub use store::{End, Insert, Kind, NewRow, RequestCounts, Row, State, Store, StoreError};

/// Default `[requests] retention_hours`: seven days.
pub const DEFAULT_RETENTION_HOURS: u64 = 168;
/// Default `[requests] max_rows`.
pub const DEFAULT_MAX_ROWS: u64 = 10_000;
/// Default `[requests] wait_max_secs`.
pub const DEFAULT_WAIT_MAX_SECS: u64 = 300;

/// The argument that names a request.
pub const REQUEST_ID: &str = "request_id";
/// The prefix of the ids the server makes.
pub const SERVER_PREFIX: &str = "srv-";

/// How often the server prunes the table while it runs.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// How often a waiter reads a row that no execution in this process owns.
const POLL: Duration = Duration::from_millis(50);
/// How long shutdown waits for executions to stop.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(60);
/// How long a client should wait before it asks again after a store failure.
const STORE_RETRY_MILLIS: u64 = 1000;
/// How often the server tries to record the end of a request.
const FINISH_ATTEMPTS: u32 = 3;
/// How often a check that waits for its worker request to stop says so.
const STOP_WARNING: Duration = Duration::from_secs(60);

/// The `[requests]` settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestSettings {
    /// How long an ended row stays.
    pub retention: Duration,
    /// The most rows the table keeps.
    pub max_rows: u64,
    /// The longest wait one `lean_get` may ask for.
    pub wait_max: Duration,
}

impl Default for RequestSettings {
    fn default() -> Self {
        Self {
            retention: Duration::from_secs(DEFAULT_RETENTION_HOURS.saturating_mul(3600)),
            max_rows: DEFAULT_MAX_ROWS,
            wait_max: Duration::from_secs(DEFAULT_WAIT_MAX_SECS),
        }
    }
}

impl RequestSettings {
    /// The settings `[requests]` gives, with the defaults for absent keys.
    ///
    /// # Errors
    /// Names a key whose value is zero or too large.
    pub fn from_file(file: &RequestsFileConfig) -> Result<Self, String> {
        let hours = file.retention_hours.unwrap_or(DEFAULT_RETENTION_HOURS);
        let max_rows = file.max_rows.unwrap_or(DEFAULT_MAX_ROWS);
        let wait_max_secs = file.wait_max_secs.unwrap_or(DEFAULT_WAIT_MAX_SECS);
        if hours == 0 || hours > 1_000_000 {
            return Err("[requests] retention_hours must be from 1 to 1000000".to_owned());
        }
        if max_rows == 0 {
            return Err("[requests] max_rows must be at least 1".to_owned());
        }
        if wait_max_secs == 0 || wait_max_secs > 86_400 {
            return Err("[requests] wait_max_secs must be from 1 to 86400".to_owned());
        }
        Ok(Self {
            retention: Duration::from_secs(hours.saturating_mul(3600)),
            max_rows,
            wait_max: Duration::from_secs(wait_max_secs),
        })
    }
}

/// A row as replies show it. Times count milliseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RowView {
    pub request_id: String,
    pub tool: String,
    pub kind: Kind,
    pub state: State,
    pub created_at: i64,
    pub deadline_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// The build log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

impl From<&Row> for RowView {
    fn from(row: &Row) -> Self {
        Self {
            request_id: row.id.clone(),
            tool: row.tool.clone(),
            kind: row.kind,
            state: row.state,
            created_at: row.created_at,
            deadline_at: row.deadline_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
            exit_code: row.exit_code,
            log: row.log_path.clone(),
        }
    }
}

/// A refusal of the request API. It makes no row and starts nothing; the
/// client gets it as a tool error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestError {
    /// A stable code, such as `request_id_conflict`.
    pub code: &'static str,
    pub message: String,
    /// Whether asking again with the same id may succeed.
    pub retryable: bool,
    /// The stored row the refusal concerns.
    pub request: Option<Box<RowView>>,
}

impl RequestError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            request: None,
        }
    }

    /// The store is unusable now; the same call may work later.
    fn store(error: &StoreError) -> Self {
        Self {
            retryable: true,
            ..Self::new("request_store_failed", error.to_string())
        }
    }

    /// The requests API needs a server started with a Lake root.
    #[must_use]
    pub fn unavailable() -> Self {
        Self::new(
            "requests_unavailable",
            "this server has no request table: start it with --lake-root to use request ids",
        )
    }

    /// The tool error that carries this refusal for request `id`.
    #[must_use]
    pub fn reply(&self, id: &str, carrier: ResponseCarrier) -> CallToolResult {
        let mut error = Map::new();
        error.insert("code".to_owned(), Value::from(self.code));
        error.insert("message".to_owned(), Value::from(self.message.clone()));
        if self.retryable {
            error.insert("retryable".to_owned(), Value::Bool(true));
            error.insert("retry_after_millis".to_owned(), Value::from(STORE_RETRY_MILLIS));
        }
        let mut body = Map::new();
        body.insert("error".to_owned(), Value::Object(error));
        body.insert(REQUEST_ID.to_owned(), Value::from(id));
        if let Some(row) = &self.request
            && let Ok(row) = serde_json::to_value(row)
        {
            body.insert("request".to_owned(), row);
        }
        let mut result = carry(&Value::Object(body), carrier);
        result.is_error = Some(true);
        result
    }
}

/// Check a client's request id: `^[A-Za-z0-9._:-]{8,128}$`, not `srv-...`.
///
/// # Errors
/// Returns `invalid_request_id` naming the rule the id breaks.
pub fn check_id(id: &str) -> Result<(), RequestError> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-');
    if !(8..=128).contains(&id.len()) || !id.chars().all(allowed) {
        return Err(RequestError::new(
            "invalid_request_id",
            "request_id must be 8 to 128 characters from A-Z, a-z, 0-9, '.', '_', ':' and '-'",
        ));
    }
    if id.starts_with(SERVER_PREFIX) {
        return Err(RequestError::new(
            "invalid_request_id",
            "request ids that start with srv- belong to the server",
        ));
    }
    Ok(())
}

/// The request id a call's `request_id` argument gives.
///
/// # Errors
/// Returns `invalid_request_id` when the argument is not a string or breaks
/// the id rule.
pub fn id_from(value: &Value) -> Result<String, RequestError> {
    let id = value
        .as_str()
        .ok_or_else(|| RequestError::new("invalid_request_id", "request_id must be a string"))?;
    check_id(id)?;
    Ok(id.to_owned())
}

/// A new id for a call that came without one: `srv-` and a random UUID.
#[must_use]
pub fn server_id() -> String {
    crate::build::new_id()
}

/// SHA-256, in hex, of the canonical JSON of `{tool, arguments}`: no spaces,
/// keys sorted at every level.
#[must_use]
pub fn intent(tool: &str, arguments: &Map<String, Value>) -> String {
    let mut whole = Map::new();
    whole.insert("arguments".to_owned(), Value::Object(arguments.clone()));
    whole.insert("tool".to_owned(), Value::from(tool));
    let mut text = String::new();
    canonical_json(&Value::Object(whole), &mut text);
    crate::digest::sha256_hex(text.as_bytes())
}

fn canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                canonical_json(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => out.push_str(&value.to_string()),
    }
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

/// The time from now until `at`, in milliseconds since the Unix epoch.
fn until(at: i64) -> Duration {
    Duration::from_millis(u64::try_from(at.saturating_sub(now_millis())).unwrap_or(0))
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Put `request_id` at the top of a tool result.
///
/// The id goes into the structured content and into the first text item when
/// that is a JSON object. An error whose text is not JSON becomes
/// `{error: {code, message}, request_id}`.
pub fn stamp_result(result: &mut CallToolResult, id: &str) {
    if let Some(Value::Object(map)) = result.structured_content.as_mut() {
        map.insert(REQUEST_ID.to_owned(), Value::from(id));
    }
    let is_error = result.is_error == Some(true);
    let Some(ContentBlock::Text(first)) = result.content.first_mut() else {
        return;
    };
    let body = match serde_json::from_str::<Value>(&first.text) {
        Ok(Value::Object(mut map)) => {
            map.insert(REQUEST_ID.to_owned(), Value::from(id));
            map
        }
        _ if is_error => {
            // rmcp reports arguments it cannot decode as plain text.
            let code = if first.text.starts_with("failed to deserialize parameters") {
                "invalid_arguments"
            } else {
                "tool_error"
            };
            let mut error = Map::new();
            error.insert("code".to_owned(), Value::from(code));
            error.insert("message".to_owned(), Value::from(first.text.as_str()));
            let mut map = Map::new();
            map.insert("error".to_owned(), Value::Object(error));
            map.insert(REQUEST_ID.to_owned(), Value::from(id));
            map
        }
        _ => return,
    };
    first.text = Value::Object(body).to_string();
}

/// Put `request_id` into a protocol error's `data`.
pub fn stamp_error(error: &mut McpError, id: &str) {
    match error.data.as_mut() {
        Some(Value::Object(map)) => {
            map.insert(REQUEST_ID.to_owned(), Value::from(id));
        }
        Some(other) => {
            let detail = other.take();
            let mut map = Map::new();
            map.insert("detail".to_owned(), detail);
            map.insert(REQUEST_ID.to_owned(), Value::from(id));
            *other = Value::Object(map);
        }
        None => {
            let mut map = Map::new();
            map.insert(REQUEST_ID.to_owned(), Value::from(id));
            error.data = Some(Value::Object(map));
        }
    }
}

/// A protocol error as a tool error: `{error: {code, message, ...data}}`.
/// The code is the data's `code` or `reason`, else the JSON-RPC code's name.
fn error_result(error: &McpError, carrier: ResponseCarrier) -> CallToolResult {
    let mut detail = Map::new();
    if let Some(Value::Object(data)) = &error.data {
        detail.extend(
            data.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    let code = detail
        .get("code")
        .or_else(|| detail.get("reason"))
        .and_then(Value::as_str)
        .map_or_else(|| rpc_code_name(error.code).to_owned(), str::to_owned);
    detail.insert("code".to_owned(), Value::from(code));
    detail.insert("message".to_owned(), Value::from(error.message.to_string()));
    let mut body = Map::new();
    body.insert("error".to_owned(), Value::Object(detail));
    let mut result = carry(&Value::Object(body), carrier);
    result.is_error = Some(true);
    result
}

fn rpc_code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::INVALID_PARAMS => "invalid_arguments",
        ErrorCode::METHOD_NOT_FOUND => "unknown_tool",
        _ => "internal_error",
    }
}

/// The work an execution reached, before the runner decides how the row ends.
enum Reached {
    /// The read turn did not come.
    Refused(ServerError),
    /// The row was not `queued` when the check got its turn.
    Lost,
    /// The store failed before the row said `running`.
    Store(StoreError),
    /// The tool ran and returned.
    Ran(Result<CallToolResult, McpError>),
}

/// Runs requests that carry an id and keeps their rows.
#[derive(Debug)]
pub struct RequestRunner {
    ctx: ToolContext,
    store: Store,
    store_dir: PathBuf,
    settings: RequestSettings,
    /// One sender per request this process runs now; it sends the final state.
    live: parking_lot::Mutex<HashMap<String, watch::Sender<State>>>,
    /// Cancelled when the server stops. Executions stop and leave their rows
    /// for recovery.
    shutdown: CancellationToken,
}

impl RequestRunner {
    /// Open the request table in the server store `store_dir` for the server
    /// whose tools `ctx` runs.
    ///
    /// # Errors
    /// Fails when the table cannot be opened.
    pub fn open(ctx: ToolContext, store_dir: &Path, settings: RequestSettings) -> Result<Arc<Self>, StoreError> {
        let store = Store::open(store_dir)?;
        Ok(Arc::new(Self {
            ctx,
            store,
            store_dir: store_dir.to_path_buf(),
            settings,
            live: parking_lot::Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        }))
    }

    /// The table.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The response carrier of the server's tools.
    pub(crate) fn carrier(&self) -> ResponseCarrier {
        self.ctx.config.carrier
    }

    /// Record the call of `tool` with `arguments` as request `id` and start
    /// it, unless a row already holds `id`. Returns the row.
    ///
    /// # Errors
    /// Refuses a bad id, an unknown tool, arguments the tool refuses, and an id
    /// whose row holds another call. None of these makes a row.
    pub async fn run(
        self: &Arc<Self>,
        id: &str,
        tool: &str,
        arguments: Map<String, Value>,
        client: Option<String>,
    ) -> Result<Row, RequestError> {
        check_id(id)?;
        // A tool that cannot run here says so first; `lean_get` takes its own
        // `request_id`, so the check below would name the wrong fault.
        let parsed = ToolCall::parse(tool, &arguments);
        if let Err(ToolCallError::UnknownTool(message)) = &parsed {
            return Err(RequestError::new("unknown_tool", message.as_str()));
        }
        if arguments.contains_key(REQUEST_ID) {
            return Err(RequestError::new(
                "invalid_arguments",
                "the tool arguments must not hold request_id; it names the request outside them",
            ));
        }
        let call = parsed.map_err(|error| match error {
            ToolCallError::UnknownTool(message) => RequestError::new("unknown_tool", message),
            ToolCallError::InvalidArguments(message) => RequestError::new("invalid_arguments", message),
        })?;
        let intent = intent(tool, &arguments);
        if let Some(stored) = self
            .store
            .get(id.to_owned())
            .await
            .map_err(|e| RequestError::store(&e))?
        {
            return same_call(stored, &intent);
        }
        let plan = self.plan(id, &call)?;
        let now = now_millis();
        let new = NewRow {
            id: id.to_owned(),
            client,
            tool: tool.to_owned(),
            intent,
            arguments: Value::Object(arguments).to_string(),
            root: plan.root,
            kind: plan.kind,
            created_at: now,
            deadline_at: now.saturating_add(millis(plan.timeout)),
            log_path: plan.log,
        };
        match self
            .store
            .insert_or_get(new)
            .await
            .map_err(|e| RequestError::store(&e))?
        {
            Insert::Inserted(row) => {
                self.start(row.clone(), call);
                Ok(row)
            }
            Insert::Existing(row) => Ok(row),
            Insert::Conflict(row) => Err(conflict(&row)),
        }
    }

    /// Fix the root, kind, deadline and log of `call` as request `id`.
    fn plan(&self, id: &str, call: &ToolCall) -> Result<Plan, RequestError> {
        let invalid = |message: String| RequestError::new("invalid_arguments", message);
        let (root, kind, timeout, log) = if let ToolCall::Build(request) = call {
            let build =
                crate::tools::build::plan_build(&self.ctx, request).map_err(|e| invalid(e.message.to_string()))?;
            let log = crate::build::log_path(&self.store_dir, id)
                .map_err(|error| RequestError::store(&StoreError::Io(error)))?;
            (build.root, Kind::Build, build.plan.timeout, Some(log))
        } else {
            let root = self
                .ctx
                .broker
                .resolve(&ProjectHint::from_request(call.project()))
                .map_err(|error| invalid(error.to_string()))?;
            let timeout = Duration::from_millis(self.ctx.broker.request_timeout_millis());
            (root, Kind::Check, timeout, None)
        };
        let text = |path: PathBuf| {
            path.into_os_string()
                .into_string()
                .map_err(|_| invalid("the project path is not UTF-8".to_owned()))
        };
        Ok(Plan {
            root: text(root)?,
            kind,
            timeout,
            log: log.map(text).transpose()?,
        })
    }

    /// Wait until request `id` ends, or for `limit` when one is given, and
    /// return its row.
    ///
    /// # Errors
    /// Returns `request_not_found` for an unknown id, and a store failure.
    pub async fn wait(&self, id: &str, limit: Option<Duration>) -> Result<Row, RequestError> {
        let until = limit.and_then(|limit| tokio::time::Instant::now().checked_add(limit));
        loop {
            // Subscribe before reading, so an end recorded after the read
            // still wakes this wait.
            let mut ended = self.live.lock().get(id).map(watch::Sender::subscribe);
            let row = self
                .store
                .get(id.to_owned())
                .await
                .map_err(|e| RequestError::store(&e))?
                .ok_or_else(|| RequestError::new("request_not_found", format!("no request has id {id}")))?;
            if row.state.is_final() || until.is_some_and(|until| tokio::time::Instant::now() >= until) {
                return Ok(row);
            }
            let pause = async {
                match ended.as_mut() {
                    // A closed channel also wakes the wait; the next read decides.
                    Some(receiver) => drop(receiver.changed().await),
                    None => tokio::time::sleep(POLL).await,
                }
            };
            match until {
                Some(until) => drop(tokio::time::timeout_at(until, pause).await),
                None => pause.await,
            }
        }
    }

    /// The longest wait `lean_get` grants for `asked` seconds.
    pub(crate) fn wait_limit(&self, asked: Option<u64>) -> Duration {
        Duration::from_secs(asked.unwrap_or(0)).min(self.settings.wait_max)
    }

    /// The number of queued and running rows.
    ///
    /// # Errors
    /// Fails when the store fails.
    pub async fn counts(&self) -> Result<RequestCounts, StoreError> {
        self.store.counts().await
    }

    /// The reply a direct call with an id gets for its ended row: the stored
    /// result, or an error naming how the request ended without one.
    pub(crate) fn stored_reply(&self, row: &Row) -> CallToolResult {
        match row.result.as_deref().map(serde_json::from_str::<CallToolResult>) {
            Some(Ok(mut result)) => {
                result.result_type = Some(ResultType::COMPLETE);
                result
            }
            Some(Err(error)) => {
                RequestError::new("request_store_failed", format!("stored result is unreadable: {error}"))
                    .reply(&row.id, self.carrier())
            }
            None => {
                let (code, message) = match row.state {
                    State::TimedOut => (
                        "request_timed_out",
                        format!("request {} reached its deadline before it started", row.id),
                    ),
                    State::Interrupted => (
                        "request_interrupted",
                        format!("a server restart ended request {} before it returned a result", row.id),
                    ),
                    State::Queued | State::Running | State::Done => {
                        ("request_not_ended", format!("request {} has no result yet", row.id))
                    }
                };
                RequestError {
                    request: Some(Box::new(RowView::from(row))),
                    ..RequestError::new(code, message)
                }
                .reply(&row.id, self.carrier())
            }
        }
    }

    /// Start the execution of `row` and the task that records its end.
    fn start(self: &Arc<Self>, row: Row, call: ToolCall) {
        let (sender, _) = watch::channel(row.state);
        self.live.lock().insert(row.id.clone(), sender);
        let id = row.id.clone();
        let work = tokio::spawn(Arc::clone(self).execute(row, call));
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let end = match work.await {
                Ok(end) => end,
                Err(error) => {
                    tracing::error!(request_id = %id, %error, "request task failed");
                    let mut result = RequestError::new("internal_error", format!("request task failed: {error}"))
                        .reply(&id, runner.carrier());
                    result.result_type = None;
                    Some(End {
                        state: State::Done,
                        finished_at: now_millis(),
                        exit_code: None,
                        result: serde_json::to_string(&result).ok(),
                    })
                }
            };
            runner.settle(&id, end).await;
        });
    }

    /// Run `row` to its end. `None` when the server stops first: the row stays
    /// as it is and recovery ends it at the next start.
    async fn execute(self: Arc<Self>, row: Row, call: ToolCall) -> Option<End> {
        let token = self.shutdown.child_token();
        let left = until(row.deadline_at);
        match call {
            ToolCall::Build(request) => self.execute_build(&row, request, &token, left).await,
            call @ (ToolCall::Semantic(_) | ToolCall::Imports(_)) => self.execute_check(&row, call, &token, left).await,
        }
    }

    async fn execute_check(&self, row: &Row, call: ToolCall, token: &CancellationToken, left: Duration) -> Option<End> {
        let semantic = call.is_semantic();
        let root = PathBuf::from(&row.root);
        let (reached, mut evidence) = with_cancellation_evidence(
            token,
            with_deadline(left, async {
                // The turn lasts until this scope ends; the tool reuses it.
                if let Err(refused) = self.ctx.broker.ensure_read(&root).await {
                    return Reached::Refused(refused);
                }
                match self.store.mark_running(row.id.clone(), now_millis()).await {
                    Ok(true) => {}
                    Ok(false) => return Reached::Lost,
                    Err(error) => return Reached::Store(error),
                }
                Reached::Ran(Box::pin(call.execute(&self.ctx, &row.id)).await)
            }),
        )
        .await;
        let stopping = token.is_cancelled();
        let late = now_millis() >= row.deadline_at;
        let result = match reached {
            Reached::Refused(_) | Reached::Lost if stopping => return None,
            Reached::Lost => {
                tracing::warn!(request_id = %row.id, "request row left the queue before it started");
                return None;
            }
            Reached::Refused(refused) if semantic => respond_semantic(Err(refused), self.carrier()),
            Reached::Refused(refused) => Err(McpError::from(refused)),
            Reached::Store(error) => Ok(RequestError::store(&error).reply(&row.id, self.carrier())),
            Reached::Ran(result) => {
                if stopping && failed(&result) {
                    return None;
                }
                result
            }
        };
        let state = if failed(&result) && late {
            // A worker request the deadline stopped may still run; the row
            // says `timed_out` only once its stop is seen.
            if !self.wait_for_stops(&row.id, &mut evidence).await {
                return None;
            }
            if evidence.uncertain || evidence.pending > 0 {
                // No observer can confirm the stop, so the row claims only
                // that the call returned.
                tracing::warn!(request_id = %row.id, "a check failed at its deadline, but its stop cannot be confirmed");
                State::Done
            } else {
                State::TimedOut
            }
        } else {
            State::Done
        };
        Some(self.end(&row.id, result, state, None))
    }

    /// Wait until the worker supervisor has seen every stop the check left
    /// open. Returns false when the server stops first.
    async fn wait_for_stops(&self, id: &str, evidence: &mut CallExecutionEvidence) -> bool {
        let started = tokio::time::Instant::now();
        let mut warned = started;
        evidence.refresh_deferred();
        while !evidence.deferred.is_empty() {
            if self.shutdown.is_cancelled() {
                return false;
            }
            if warned.elapsed() >= STOP_WARNING {
                warned = tokio::time::Instant::now();
                tracing::warn!(request_id = %id, waited_secs = started.elapsed().as_secs(), "a timed-out check still waits for its worker request to stop");
            }
            tokio::time::sleep(POLL).await;
            evidence.refresh_deferred();
        }
        true
    }

    async fn execute_build(
        &self,
        row: &Row,
        request: crate::build::BuildRequest,
        token: &CancellationToken,
        left: Duration,
    ) -> Option<End> {
        let mut build = match crate::tools::build::plan_build(&self.ctx, &request) {
            Ok(build) => build,
            Err(error) => return Some(self.end(&row.id, Err(error), State::Done, None)),
        };
        build.plan.timeout = left;
        let Some(log) = row.log_path.as_deref().map(PathBuf::from) else {
            let error = McpError::internal_error("build request has no log path", None);
            return Some(self.end(&row.id, Err(error), State::Done, None));
        };
        let store = self.store.clone();
        let id = row.id.clone();
        let on_running: OnRunning = Box::new(move || match store.mark_running_blocking(&id, now_millis()) {
            Ok(true) => Ok(()),
            Ok(false) => Err("the request row is not queued".to_owned()),
            Err(error) => Err(error.to_string()),
        });
        let outcome = with_cancellation(
            token,
            crate::tools::build::run_planned(&self.ctx, build, &row.id, &log, Some(on_running)),
        )
        .await;
        match outcome {
            Ok(outcome) => {
                let state = match outcome.state {
                    BuildState::TimedOut => State::TimedOut,
                    BuildState::Done => State::Done,
                };
                let mut result = carry(&outcome, self.carrier());
                result.is_error = Some(!outcome.ok);
                Some(self.end(&row.id, Ok(result), state, outcome.exit_code.map(i64::from)))
            }
            Err(_) if token.is_cancelled() => None,
            Err(error) => {
                let state = if now_millis() >= row.deadline_at {
                    State::TimedOut
                } else {
                    State::Done
                };
                Some(self.end(&row.id, Err(error), state, None))
            }
        }
    }

    /// The end of request `id` with `result`, stamped and ready to store.
    fn end(&self, id: &str, result: Result<CallToolResult, McpError>, state: State, exit_code: Option<i64>) -> End {
        let mut result = result.unwrap_or_else(|error| error_result(&error, self.carrier()));
        stamp_result(&mut result, id);
        // A reply sets the result type for its own peer.
        result.result_type = None;
        let stored = serde_json::to_string(&result).unwrap_or_else(|error| {
            let mut fallback = RequestError::new("internal_error", format!("cannot store the result: {error}"))
                .reply(id, self.carrier());
            fallback.result_type = None;
            serde_json::to_string(&fallback).unwrap_or_default()
        });
        End {
            state,
            finished_at: now_millis(),
            exit_code,
            result: Some(stored),
        }
    }

    /// Record `end` for request `id`, wake its waiters and forget it.
    async fn settle(&self, id: &str, end: Option<End>) {
        if let Some(end) = end {
            let state = end.state;
            let mut attempt = 1;
            loop {
                match self.store.finish(id.to_owned(), end.clone()).await {
                    Ok(true) => break,
                    Ok(false) => {
                        tracing::warn!(request_id = %id, "request row had already ended");
                        break;
                    }
                    Err(error) if attempt < FINISH_ATTEMPTS => {
                        tracing::warn!(request_id = %id, %error, "cannot record the end of a request; trying again");
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(error) => {
                        tracing::error!(request_id = %id, %error, "cannot record the end of a request; the next start ends it");
                        break;
                    }
                }
            }
            if let Some(sender) = self.live.lock().get(id) {
                sender.send_replace(state);
            }
        }
        self.live.lock().remove(id);
    }

    /// End request `id` now with no result, for recovery.
    async fn end_unrun(&self, id: &str, state: State) -> Result<(), StoreError> {
        let end = End {
            state,
            finished_at: now_millis(),
            exit_code: None,
            result: None,
        };
        self.store.finish(id.to_owned(), end).await.map(drop)
    }

    /// Prune the table every hour until the server stops.
    pub fn start_pruning(self: &Arc<Self>) {
        let runner = Arc::downgrade(self);
        let stop = self.shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PRUNE_EVERY);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick is now; the server pruned at its start.
            ticker.tick().await;
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = ticker.tick() => {}
                }
                let Some(runner) = runner.upgrade() else {
                    return;
                };
                runner.prune(false).await;
            }
        });
    }

    /// Delete ended rows past retention and past the row cap, then their logs.
    /// At start, also delete every log no row names; later, only those older
    /// than the retention, since a build without an id may write one now.
    async fn prune(&self, at_start: bool) {
        let before = now_millis().saturating_sub(millis(self.settings.retention));
        match self.store.prune(before, self.settings.max_rows).await {
            Ok(logs) => {
                for log in logs {
                    remove_log(&log);
                }
            }
            Err(error) => tracing::warn!(%error, "cannot prune the request table"),
        }
        let named = match self.store.log_paths().await {
            Ok(named) => named,
            Err(error) => {
                tracing::warn!(%error, "cannot list the request logs");
                return;
            }
        };
        let Ok(logs) = self.store_dir.join("logs").canonicalize() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&logs) else {
            return;
        };
        let cutoff = SystemTime::now().checked_sub(self.settings.retention);
        for entry in entries.flatten() {
            let path = logs.join(entry.file_name());
            if named.contains(&path) || path.extension().is_none_or(|extension| extension != "log") {
                continue;
            }
            let old = at_start
                || entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| cutoff.is_some_and(|cutoff| modified < cutoff));
            if old {
                remove_log(&path);
            }
        }
    }

    /// Stop every execution and wait, for a bounded time, until each has
    /// stopped. Rows that have not ended stay for recovery at the next start.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let started = tokio::time::Instant::now();
        while !self.live.lock().is_empty() {
            if started.elapsed() >= SHUTDOWN_WAIT {
                tracing::warn!("requests still run at shutdown; the next start ends them");
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

/// Whether a tool call failed: a protocol error or a result with `isError`.
fn failed(result: &Result<CallToolResult, McpError>) -> bool {
    result.as_ref().map_or(true, |result| result.is_error == Some(true))
}

/// What [`RequestRunner::plan`] fixes for a new row.
struct Plan {
    root: String,
    kind: Kind,
    timeout: Duration,
    log: Option<String>,
}

/// `stored` when it holds the call with `intent`, else the conflict.
fn same_call(stored: Row, intent: &str) -> Result<Row, RequestError> {
    if stored.intent == intent {
        Ok(stored)
    } else {
        Err(conflict(&stored))
    }
}

fn conflict(stored: &Row) -> RequestError {
    RequestError {
        request: Some(Box::new(RowView::from(stored))),
        ..RequestError::new(
            "request_id_conflict",
            format!(
                "request {} already holds another call: {} ({})",
                stored.id,
                stored.tool,
                stored.state.as_str()
            ),
        )
    }
}

fn remove_log(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(log = %path.display(), %error, "cannot delete a request log"),
    }
}
