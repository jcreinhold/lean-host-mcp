//! Bounded retained requests. The optional journal stores evidence, never work to
//! replay. A fresh admission epoch fences forgotten IDs without cycling workers.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, DetailedTask, Task, TaskPayload, TaskStatus};
use rmcp::task_manager::current_timestamp;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::cancellation::with_cancellation_evidence;
use crate::request_journal::RequestJournal;

const MAX_TASKS: usize = 128;
const RETENTION: Duration = Duration::from_hours(24);
const LEGACY_RETENTION: Duration = Duration::from_secs(600);
pub(crate) const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestTasks(Arc<Mutex<Registry>>);

#[derive(Debug)]
struct Registry {
    entries: HashMap<String, Entry>,
    epoch: String,
    journal: Option<RequestJournal>,
    journal_error: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            epoch: uuid::Uuid::new_v4().to_string(),
            journal: None,
            journal_error: false,
        }
    }
}

#[derive(Debug)]
struct Entry {
    stored: StoredRequest,
    cancellation: CancellationToken,
    created: Instant,
    expires: Option<Instant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredRequest {
    task: DetailedTask,
    request: Option<RequestBinding>,
    execution: Execution,
    cancellation_requested: bool,
    acknowledged: bool,
    result_available: bool,
    #[serde(default)]
    finished_at: Option<u64>,
}

impl StoredRequest {
    pub(crate) fn id(&self) -> &str {
        &self.task.task.task_id
    }
    pub(crate) fn is_epoch_bound(&self) -> bool {
        self.request.as_ref().and_then(|r| r.request_epoch.as_ref()).is_some()
    }
    fn retention(&self) -> Duration {
        if self.is_epoch_bound() {
            RETENTION
        } else {
            LEGACY_RETENTION
        }
    }
}

/// Execution lifetime is independent of the proof verdict and cancellation intent.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Execution {
    disposition: Disposition,
    reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Disposition {
    Pending,
    NotStarted,
    Stopped,
    Unknown,
}

impl Execution {
    fn new(disposition: Disposition, reason: &str) -> Self {
        Self {
            disposition,
            reason: reason.to_owned(),
        }
    }
}

/// Immutable input identity, captured atomically with explicit submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RequestBinding {
    pub server_instance_id: String,
    pub tool: String,
    pub submission_sha256: String,
    pub project: Option<String>,
    /// Required for v2 retry safety; omitted by legacy clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_epoch: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RetainedTask {
    #[serde(flatten)]
    fields: serde_json::Map<String, serde_json::Value>,
    request: Option<RequestBinding>,
    execution: Execution,
    cancellation_requested: bool,
    acknowledged: bool,
    result_available: bool,
}

impl RequestTasks {
    pub(crate) fn open(path: &Path) -> std::io::Result<Self> {
        let (journal, stored) = RequestJournal::open(path, MAX_TASKS)?;
        let mut registry = Registry {
            journal: Some(journal),
            ..Registry::default()
        };
        for mut record in stored {
            if record.execution.disposition == Disposition::Pending {
                // Accepted is synced before spawn; dispatch_intent is synced
                // before invoking any tool code. Neither state is relaunched.
                record.execution = if record.execution.reason == "accepted" {
                    Execution::new(Disposition::NotStarted, "broker_restarted_before_dispatch")
                } else {
                    Execution::new(Disposition::Unknown, "broker_restarted_during_execution")
                };
                record.task = failed_task(
                    record.task.task,
                    "broker_restarted",
                    "The previous broker stopped; work was not replayed",
                );
                record.result_available = false;
                record.finished_at = Some(unix_seconds());
                if let Err(error) = registry.persist(&record) {
                    tracing::warn!(%error, "could not save interrupted request; will retry before new work");
                }
            }
            if record.task.status().is_terminal() && record.finished_at.is_none() {
                record.finished_at = Some(unix_seconds());
                if let Err(error) = registry.persist(&record) {
                    tracing::warn!(%error, "could not save result retention time; will retry");
                }
            }
            let remaining = record.task.status().is_terminal().then(|| {
                RETENTION.saturating_sub(Duration::from_secs(
                    unix_seconds().saturating_sub(*record.finished_at.get_or_insert_with(unix_seconds)),
                ))
            });
            let id = record.task.task.task_id.clone();
            registry.entries.insert(
                id,
                Entry {
                    stored: record,
                    cancellation: CancellationToken::new(),
                    created: Instant::now(),
                    expires: remaining.and_then(|duration| Instant::now().checked_add(duration)),
                },
            );
        }
        Ok(Self(Arc::new(Mutex::new(registry))))
    }

    pub(crate) fn status(&self) -> crate::status::RequestRegistryStatus {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        let active = registry
            .entries
            .values()
            .filter(|e| !e.stored.task.status().is_terminal())
            .count();
        crate::status::RequestRegistryStatus {
            active,
            retained_terminal: registry.entries.len().saturating_sub(active),
            capacity: MAX_TASKS,
            terminal_retention_millis: u64::try_from(RETENTION.as_millis()).unwrap_or(u64::MAX),
            request_epoch: registry.epoch.clone(),
            durable: registry.journal.is_some() && !registry.journal_error,
            journal_healthy: !registry.journal_error,
            max_record_bytes: MAX_RECORD_BYTES,
        }
    }

    pub(crate) fn spawn<F, Fut>(&self, tool: &str, operation: F) -> Option<Task>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
    {
        self.spawn_inner(tool, None, operation).ok().flatten()
    }

    pub(crate) fn submit<F, Fut>(
        &self,
        tool: &str,
        request_id: String,
        submission: RequestBinding,
        operation: F,
    ) -> Result<Task, McpError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
    {
        if request_id.is_empty()
            || request_id.len() > 128
            || !request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        {
            return Err(no_start(
                "invalid_request_id",
                "request_id must contain 1-128 ASCII letters, digits, underscores, hyphens, dots or colons",
            ));
        }
        self.spawn_inner(tool, Some((request_id, submission)), operation)?
            .ok_or_else(|| no_start("request_registry_full", "Request registry full; no work was submitted"))
    }

    fn spawn_inner<F, Fut>(
        &self,
        tool: &str,
        identity: Option<(String, RequestBinding)>,
        operation: F,
    ) -> Result<Option<Task>, McpError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
    {
        let cancellation = CancellationToken::new();
        let task = {
            let mut registry = self.0.lock();
            registry.prune(Instant::now());
            if let Some((id, submission)) = &identity {
                if let Some(entry) = registry.entries.get(id) {
                    // Origin instance is evidence, not part of retry equality:
                    // an exact retained input can be read after a restart.
                    let mut original = entry.stored.request.clone();
                    if let Some(original) = &mut original {
                        original.server_instance_id.clone_from(&submission.server_instance_id);
                    }
                    return if original.as_ref() == Some(submission) {
                        Ok(Some(entry.stored.task.task.clone()))
                    } else {
                        Err(McpError::invalid_params(
                            "request_id already belongs to a different submission",
                            Some(serde_json::json!({"reason":"request_identity_conflict"})),
                        ))
                    };
                }
                if let Some(epoch) = &submission.request_epoch
                    && epoch != &registry.epoch
                {
                    // Refusing this retry says nothing about the old lifetime.
                    return Err(unknown_request("request_epoch_closed"));
                }
            }
            // Qualify identity and epoch BEFORE compaction. Otherwise the new
            // request that reclaims capacity would invalidate its own observed
            // epoch. Existing identities return above without being forgotten;
            // this absent ID was fresh in the still-open epoch under this lock.
            // Rotation now fences forgotten keys without relaunching any of them.
            registry.compact();
            registry.recover_journal();
            if registry.journal_error {
                return Err(no_start(
                    "request_journal_unavailable",
                    "Request journal unavailable; no work was submitted",
                ));
            }
            if registry.entries.len() >= MAX_TASKS {
                return Ok(None);
            }
            let (task_id, request) = identity.map_or_else(
                || (uuid::Uuid::new_v4().to_string(), None),
                |(id, request)| (id, Some(request)),
            );
            let now = current_timestamp();
            let task = Task::new(task_id, TaskStatus::Working, &now, &now)
                .with_status_message(format!("Accepted {tool}"))
                .with_poll_interval_ms(2_000);
            let mut stored = StoredRequest {
                task: DetailedTask::new(task.clone(), TaskPayload::Working),
                request,
                execution: Execution::new(Disposition::Pending, "accepted"),
                cancellation_requested: false,
                acknowledged: false,
                result_available: false,
                finished_at: None,
            };
            if registry.persist(&stored).is_err() {
                // Rename may have succeeded before directory sync failed. Keep
                // this ID accounted for and retry its no-start result on recovery.
                stored.execution = Execution::new(Disposition::NotStarted, "request_journal_unavailable");
                stored.task = failed_task(task.clone(), "request_journal_unavailable", "Work was not submitted");
                stored.finished_at = Some(unix_seconds());
                registry.entries.insert(
                    task.task_id,
                    Entry {
                        stored,
                        cancellation,
                        created: Instant::now(),
                        expires: Instant::now().checked_add(RETENTION),
                    },
                );
                return Err(no_start(
                    "request_journal_unavailable",
                    "Could not retain acceptance; no work was submitted",
                ));
            }
            registry.entries.insert(
                task.task_id.clone(),
                Entry {
                    stored,
                    cancellation: cancellation.clone(),
                    created: Instant::now(),
                    expires: None,
                },
            );
            task
        };
        let tasks = self.clone();
        let task_id = task.task_id.clone();
        tokio::spawn(async move {
            if !tasks.start(&task_id) {
                return;
            }
            let result =
                tokio::spawn(async move { with_cancellation_evidence(&cancellation, operation()).await }).await;
            match result {
                Ok((result, uncertain)) => tasks.finish(&task_id, result, false, uncertain),
                Err(error) => tasks.finish(
                    &task_id,
                    Err(McpError::internal_error(format!("task failed: {error}"), None)),
                    true,
                    true,
                ),
            }
            // Reads and admissions prune expired results. Avoid a sleeping task
            // per completed call: evicted results would otherwise leave timers
            // behind for the whole retention period.
        });
        Ok(Some(task))
    }

    fn start(&self, id: &str) -> bool {
        let mut registry = self.0.lock();
        let Some(entry) = registry.entries.get_mut(id) else {
            return false;
        };
        let cancelled = entry.cancellation.is_cancelled();
        if cancelled {
            entry.stored.execution = Execution::new(Disposition::NotStarted, "cancelled_before_dispatch");
            entry.stored.task = DetailedTask::new(entry.stored.task.task.clone(), TaskPayload::Cancelled);
            entry.stored.task.task.last_updated_at = current_timestamp();
            entry.stored.task.task.status_message = None;
            entry.stored.finished_at = Some(unix_seconds());
            {
                entry.expires = Instant::now().checked_add(entry.stored.retention());
                entry.stored.task.task.ttl_ms = Some(
                    u64::try_from(
                        entry
                            .created
                            .elapsed()
                            .saturating_add(entry.stored.retention())
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                );
            }
        } else {
            "dispatch_intent".clone_into(&mut entry.stored.execution.reason);
        }
        let record = entry.stored.clone();
        if registry.persist(&record).is_err() {
            if let Some(entry) = registry.entries.get_mut(id) {
                entry.stored.execution = Execution::new(Disposition::NotStarted, "request_journal_unavailable");
                entry.stored.finished_at = Some(unix_seconds());
                entry.expires = Instant::now().checked_add(entry.stored.retention());
                entry.stored.task = failed_task(
                    entry.stored.task.task.clone(),
                    "request_journal_unavailable",
                    "Could not retain dispatch intent; work was not started",
                );
            }
            return false;
        }
        drop(registry);
        !cancelled
    }

    pub(crate) fn get(&self, id: &str) -> Result<DetailedTask, McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        registry
            .entries
            .get(id)
            .filter(|e| !e.stored.acknowledged)
            .map(|e| e.stored.task.clone())
            .ok_or_else(unknown_task)
    }

    pub(crate) fn get_retained(&self, id: &str) -> Result<RetainedTask, McpError> {
        self.get_retained_scoped(id, None)
    }

    pub(crate) fn get_retained_scoped(&self, id: &str, epoch: Option<&str>) -> Result<RetainedTask, McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        let entry = registry.entries.get(id).ok_or_else(unknown_task)?;
        check_epoch(entry, epoch)?;
        let entry = entry.stored.clone();
        drop(registry);
        retained_snapshot(entry)
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<(), McpError> {
        self.cancel_scoped(id, None)
    }

    pub(crate) fn cancel_scoped(&self, id: &str, epoch: Option<&str>) -> Result<(), McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        let entry = registry.entries.get_mut(id).ok_or_else(unknown_task)?;
        check_epoch(entry, epoch)?;
        if !entry.stored.task.status().is_terminal() {
            // Intent is retained before acknowledgement; signalling remains safe
            // even if disk failure prevents us acknowledging the intent.
            entry.cancellation.cancel();
            entry.stored.cancellation_requested = true;
            entry.stored.task.task.status_message = Some("Cancellation requested; waiting for work to stop".to_owned());
            entry.stored.task.task.last_updated_at = current_timestamp();
            let record = entry.stored.clone();
            registry
                .persist(&record)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        drop(registry);
        Ok(())
    }

    pub(crate) fn acknowledge(&self, id: &str, epoch: &str, digest: &str) -> Result<RetainedTask, McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        registry.recover_journal();
        if registry.journal_error {
            return Err(McpError::internal_error("Request journal unavailable", None));
        }
        let entry = registry.entries.get_mut(id).ok_or_else(unknown_task)?;
        let request = entry.stored.request.as_ref().ok_or_else(unknown_task)?;
        if request.request_epoch.as_deref() != Some(epoch) || request.submission_sha256 != digest {
            return Err(McpError::invalid_params("Acknowledgment identity mismatch", None));
        }
        if !entry.stored.task.status().is_terminal() || entry.stored.execution.disposition == Disposition::Pending {
            return Err(McpError::invalid_params("Cannot acknowledge a pending request", None));
        }
        let mut record = entry.stored.clone();
        record.acknowledged = true;
        record.result_available = false;
        match &mut record.task.payload {
            TaskPayload::Completed { result } => result.clear(),
            TaskPayload::Failed { error } => error.clear(),
            TaskPayload::Working | TaskPayload::InputRequired { .. } | TaskPayload::Cancelled => {}
            _ => {}
        }
        registry
            .persist(&record)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if let Some(entry) = registry.entries.get_mut(id) {
            entry.stored = record.clone();
        }
        drop(registry);
        retained_snapshot(record)
    }

    fn finish(&self, id: &str, result: Result<CallToolResult, McpError>, panicked: bool, uncertain: bool) {
        let mut registry = self.0.lock();
        let Some(entry) = registry.entries.get_mut(id) else {
            return;
        };
        // A cancellation signal is not a result. In particular it must never
        // overwrite a real proof result returned while cancellation raced it.
        entry.stored.execution = if uncertain && !panicked {
            Execution::new(Disposition::Unknown, "worker_operation_failed_without_exit_witness")
        } else {
            completion_execution(&result, panicked)
        };
        let payload = match result {
            Ok(result) => match serde_json::to_value(result) {
                Ok(serde_json::Value::Object(result)) => TaskPayload::Completed { result },
                _ => TaskPayload::Failed {
                    error: error_map("result_serialization_failed", "Could not serialize tool result"),
                },
            },
            Err(error) => TaskPayload::Failed {
                error: serde_json::Map::from_iter([
                    ("code".to_owned(), serde_json::json!(error.code)),
                    ("message".to_owned(), serde_json::json!(error.message)),
                    ("data".to_owned(), serde_json::json!(error.data)),
                ]),
            },
        };
        entry.stored.task.task.last_updated_at = current_timestamp();
        entry.stored.task.task.status_message = None;
        entry.stored.finished_at = Some(unix_seconds());
        entry.stored.task.task.ttl_ms = Some(
            u64::try_from(
                entry
                    .created
                    .elapsed()
                    .saturating_add(entry.stored.retention())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
        );
        entry.expires = Instant::now().checked_add(entry.stored.retention());
        entry.stored.task = DetailedTask::new(entry.stored.task.task.clone(), payload);
        entry.stored.result_available = true;
        if serde_json::to_vec(&entry.stored).is_ok_and(|bytes| bytes.len() > MAX_RECORD_BYTES.saturating_sub(64)) {
            entry.stored.task = failed_task(
                entry.stored.task.task.clone(),
                "result_too_large",
                "Result exceeds retained record limit; no semantic verdict is available",
            );
            entry.stored.result_available = false;
        }
        let record = entry.stored.clone();
        if let Err(error) = registry.persist(&record) {
            tracing::error!(%error, "could not journal terminal request; new admission disabled");
        }
    }
}

impl Registry {
    fn persist(&mut self, record: &StoredRequest) -> std::io::Result<()> {
        // Negotiated MCP tasks and legacy submissions retain their original
        // process-local contract. Only epoch-bound v2 records are journalled.
        if record.request.as_ref().and_then(|r| r.request_epoch.as_ref()).is_some()
            && let Some(journal) = &self.journal
            && let Err(error) = journal.write(&record.task.task.task_id, record)
        {
            self.journal_error = true;
            return Err(error);
        }
        Ok(())
    }

    // Retry on demand, under the registry lock. Re-save all current records before
    // admitting work: a prior terminal write may have failed after execution.
    fn recover_journal(&mut self) {
        if !self.journal_error {
            return;
        }
        let Some(journal) = &self.journal else {
            return;
        };
        if journal.check_writable().is_err() {
            return;
        }
        for entry in self.entries.values().filter(|e| e.stored.is_epoch_bound()) {
            if journal.write(entry.stored.id(), &entry.stored).is_err() {
                return;
            }
        }
        self.journal_error = false;
    }

    fn forget(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        // Rotation precedes removal, including partial disk failures. Restart
        // also creates a fresh epoch, so forgotten IDs cannot execute again.
        self.epoch = uuid::Uuid::new_v4().to_string();
        for id in ids {
            if let Some(journal) = &self.journal
                && let Err(error) = journal.remove(&id)
            {
                self.journal_error = true;
                tracing::warn!(%error, "could not remove expired result; will retry");
                continue;
            }
            self.entries.remove(&id);
        }
    }

    fn prune(&mut self, now: Instant) {
        let ids = self
            .entries
            .iter()
            .filter(|(_, e)| e.stored.task.status().is_terminal() && e.expires.is_some_and(|expiry| expiry <= now))
            .map(|(id, _)| id.clone())
            .collect();
        self.forget(ids);
    }

    fn compact(&mut self) {
        if self.entries.len() < MAX_TASKS {
            return;
        }
        // Prefer acknowledged records, then the oldest terminal result. Legacy
        // IDs have no epoch and cannot safely be pressure-evicted.
        let id = self
            .entries
            .iter()
            .filter(|(_, e)| e.stored.is_epoch_bound() && e.stored.task.status().is_terminal())
            .min_by_key(|(_, e)| (!e.stored.acknowledged, e.stored.finished_at, e.created))
            .map(|(id, _)| id.clone());
        self.forget(id.into_iter().collect());
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn completion_execution(result: &Result<CallToolResult, McpError>, panicked: bool) -> Execution {
    if panicked {
        return Execution::new(Disposition::Unknown, "task_panicked");
    }
    match result {
        Ok(result) => {
            // Runtime-unavailable envelopes can hide an uncertain cleanup. Do
            // not infer an exit witness from a transport-level successful call.
            let unavailable =
                result.is_error == Some(true)
                    || result.structured_content.as_ref().is_some_and(runtime_unavailable)
                    || result.content.iter().filter_map(|c| c.as_text()).any(|t| {
                        serde_json::from_str::<serde_json::Value>(&t.text).is_ok_and(|v| runtime_unavailable(&v))
                    });
            if unavailable {
                Execution::new(Disposition::Unknown, "runtime_unavailable")
            } else {
                Execution::new(Disposition::Stopped, "operation_returned")
            }
        }
        Err(error) => {
            if let Some(execution) = error.data.as_ref().and_then(|d| d.get("execution"))
                && execution.get("disposition").and_then(serde_json::Value::as_str) == Some("not_started")
            {
                return Execution::new(Disposition::NotStarted, "admission_refused");
            }
            Execution::new(Disposition::Unknown, "operation_failed_without_exit_witness")
        }
    }
}

fn retained_snapshot(entry: StoredRequest) -> Result<RetainedTask, McpError> {
    let serde_json::Value::Object(mut fields) =
        serde_json::to_value(&entry.task).map_err(|e| McpError::internal_error(e.to_string(), None))?
    else {
        return Err(unknown_task());
    };
    if entry.acknowledged {
        fields.remove("result");
        fields.remove("error");
    }
    Ok(RetainedTask {
        fields,
        request: entry.request,
        execution: entry.execution,
        cancellation_requested: entry.cancellation_requested,
        acknowledged: entry.acknowledged,
        result_available: entry.result_available,
    })
}

fn check_epoch(entry: &Entry, epoch: Option<&str>) -> Result<(), McpError> {
    if let Some(epoch) = epoch
        && entry.stored.request.as_ref().and_then(|r| r.request_epoch.as_deref()) != Some(epoch)
    {
        return Err(unknown_request("request_epoch_mismatch"));
    }
    Ok(())
}

fn runtime_unavailable(value: &serde_json::Value) -> bool {
    value.get("status").and_then(serde_json::Value::as_str) == Some("runtime_unavailable")
        || value
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|errors| {
                errors
                    .iter()
                    .any(|e| e.get("code").and_then(serde_json::Value::as_str) == Some("runtime_unavailable"))
            })
}

pub(crate) fn no_start(reason: &str, message: &str) -> McpError {
    McpError::invalid_params(
        message.to_owned(),
        Some(serde_json::json!({"reason":reason,"execution":{"disposition":"not_started","reason":reason}})),
    )
}
fn unknown_request(reason: &str) -> McpError {
    McpError::invalid_params(
        "Unknown, expired or closed-epoch request; no retry was dispatched",
        Some(serde_json::json!({"reason":reason,"execution":{"disposition":"unknown","reason":reason}})),
    )
}
fn unknown_task() -> McpError {
    unknown_request("request_outcome_unavailable")
}
fn error_map(reason: &str, message: &str) -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::from_iter([
        ("code".into(), serde_json::json!(-32603)),
        ("message".into(), serde_json::json!(message)),
        ("data".into(), serde_json::json!({"reason":reason})),
    ])
}
fn failed_task(task: Task, reason: &str, message: &str) -> DetailedTask {
    DetailedTask::new(
        task,
        TaskPayload::Failed {
            error: error_map(reason, message),
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn binding(byte: u8) -> RequestBinding {
        RequestBinding {
            server_instance_id: "test-instance".to_owned(),
            tool: "test".to_owned(),
            submission_sha256: format!("{byte:02x}").repeat(32),
            project: Some("/test/project".to_owned()),
            request_epoch: None,
        }
    }

    async fn terminal(tasks: &RequestTasks, task_id: &str) -> DetailedTask {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let task = tasks.get(task_id).unwrap();
                if task.status().is_terminal() {
                    return task;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn identified_retry_returns_original_result_and_rejects_changed_inputs() {
        let tasks = RequestTasks::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let original = tasks
            .submit("test", "client-chosen-id".into(), binding(1), move || async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let retry = tasks
            .submit("test", "client-chosen-id".into(), binding(1), || async {
                panic!("retry must not execute")
            })
            .unwrap();
        assert_eq!(original.task_id, retry.task_id);
        let completed = terminal(&tasks, &original.task_id).await;
        assert_eq!(completed.status(), TaskStatus::Completed);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            tasks
                .submit("test", "client-chosen-id".into(), binding(2), || async {
                    panic!("conflict must not execute")
                })
                .is_err()
        );
        let retry = tasks
            .submit("test", "client-chosen-id".into(), binding(1), || async {
                panic!("completed retry must not execute")
            })
            .unwrap();
        assert_eq!(retry.status, TaskStatus::Completed);
        assert_eq!(tasks.get(&retry.task_id).unwrap(), completed);
        let retained = tasks.get_retained(&retry.task_id).unwrap();
        assert_eq!(retained.request, Some(binding(1)));
        tasks.cancel(&retry.task_id).unwrap();
        assert_eq!(tasks.get_retained(&retry.task_id).unwrap().request, retained.request);
        assert_eq!(tasks.status().active, 0);
        assert_eq!(tasks.status().retained_terminal, 1);
    }

    #[tokio::test]
    async fn identified_capacity_rejection_does_not_execute_or_evict() {
        let tasks = RequestTasks::default();
        for index in 0..MAX_TASKS {
            tasks
                .submit("test", format!("id-{index}"), binding(1), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
        }
        assert!(
            tasks
                .submit("test", "overflow".into(), binding(1), || async {
                    panic!("overflow must not execute")
                })
                .is_err()
        );
        assert!(
            tasks
                .submit("test", "id-0".into(), binding(1), || async {
                    panic!("retry must not execute even when full")
                })
                .is_ok()
        );
        assert!(tasks.get("overflow").is_err());
        assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
    }

    #[tokio::test]
    async fn immutable_binding_is_serialized_with_every_retained_state() {
        let tasks = RequestTasks::default();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let working = tasks
            .submit("test", "working-fixture".into(), binding(1), || async {
                finish_rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let observe = |id: &str, status: &str| {
            let value = serde_json::to_value(tasks.get_retained(id).unwrap()).unwrap();
            assert_eq!(value.get("taskId").and_then(serde_json::Value::as_str), Some(id));
            assert_eq!(value.get("status").and_then(serde_json::Value::as_str), Some(status));
            assert_eq!(value.get("request"), Some(&serde_json::to_value(binding(1)).unwrap()));
            eprintln!("RETAINED_FIXTURE {status} {value}");
        };
        observe(&working.task_id, "working");
        finish_tx.send(()).unwrap();
        terminal(&tasks, &working.task_id).await;
        observe(&working.task_id, "completed");
        let failed = tasks
            .submit("test", "failed-fixture".into(), binding(1), || async {
                Err(McpError::invalid_params("test failure", None))
            })
            .unwrap();
        terminal(&tasks, &failed.task_id).await;
        observe(&failed.task_id, "failed");
        let cancelled = tasks
            .submit("test", "cancelled-fixture".into(), binding(1), || async {
                crate::cancellation::CallCancellation::current().cancelled().await;
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        tasks.cancel(&cancelled.task_id).unwrap();
        terminal(&tasks, &cancelled.task_id).await;
        observe(&cancelled.task_id, "cancelled");
    }

    #[tokio::test]
    async fn cancellation_ack_waits_for_operation_cleanup() {
        let tasks = RequestTasks::default();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let task = tasks
            .spawn("test", move || async move {
                let cancellation = crate::cancellation::CallCancellation::current();
                started_tx.send(()).unwrap();
                cancellation.cancelled().await;
                finish_rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        started_rx.await.unwrap();
        tasks.cancel(&task.task_id).unwrap();
        assert_eq!(tasks.get(&task.task_id).unwrap().status(), TaskStatus::Working);
        finish_tx.send(()).unwrap();
        assert_eq!(terminal(&tasks, &task.task_id).await.status(), TaskStatus::Completed);
        assert!(tasks.get_retained(&task.task_id).unwrap().cancellation_requested);
    }

    #[tokio::test]
    async fn retained_result_survives_handle_clone_and_expires_only_after_completion() {
        let tasks = RequestTasks::default();
        let clone = tasks.clone();
        let task = tasks
            .spawn("test", || async { Ok(CallToolResult::error(vec![])) })
            .unwrap();
        assert!(tasks.get(&task.task_id).is_ok());
        drop(tasks);
        let result = terminal(&clone, &task.task_id).await;
        assert_eq!(result.status(), TaskStatus::Completed);
        let TaskPayload::Completed { result } = result.payload else {
            panic!("expected result")
        };
        assert_eq!(result.get("isError"), Some(&serde_json::json!(true)));
        clone
            .0
            .lock()
            .prune(Instant::now() + RETENTION + Duration::from_secs(1));
        assert!(clone.get(&task.task_id).is_err());
    }

    #[tokio::test]
    async fn registry_limit_never_evicts_active_or_retained_requests() {
        let tasks = RequestTasks::default();
        for _ in 0..MAX_TASKS {
            assert!(
                tasks
                    .spawn("test", || async { Ok(CallToolResult::success(vec![])) })
                    .is_some()
            );
        }
        assert!(
            tasks
                .spawn("overflow", || async { Ok(CallToolResult::success(vec![])) })
                .is_none()
        );
        assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
    }

    #[tokio::test]
    async fn immediate_cancel_is_observed_before_operation_starts() {
        let tasks = RequestTasks::default();
        let task = tasks
            .spawn("test", || async {
                assert!(crate::cancellation::CallCancellation::current().is_cancelled());
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        tasks.cancel(&task.task_id).unwrap();
        assert_eq!(terminal(&tasks, &task.task_id).await.status(), TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn protocol_error_is_retained_and_active_tasks_do_not_expire() {
        let tasks = RequestTasks::default();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let task = tasks
            .spawn("test", move || async move {
                finish_rx.await.unwrap();
                Err(McpError::invalid_params(
                    "invalid project",
                    Some(serde_json::json!({"detail": 1})),
                ))
            })
            .unwrap();
        tasks
            .0
            .lock()
            .prune(Instant::now() + RETENTION + Duration::from_secs(1));
        assert_eq!(tasks.get(&task.task_id).unwrap().status(), TaskStatus::Working);
        finish_tx.send(()).unwrap();
        let TaskPayload::Failed { error } = terminal(&tasks, &task.task_id).await.payload else {
            panic!("expected protocol error")
        };
        assert_eq!(error.get("message"), Some(&serde_json::json!("invalid project")));
        assert_eq!(error.get("data"), Some(&serde_json::json!({"detail": 1})));
    }
    fn v2_binding(tasks: &RequestTasks) -> RequestBinding {
        RequestBinding {
            request_epoch: Some(tasks.status().request_epoch),
            ..binding(1)
        }
    }

    #[tokio::test]
    async fn v2_results_survive_until_expiry_and_ack_requires_exact_consumed_identity() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        let epoch = binding.request_epoch.clone().unwrap();
        tasks
            .submit("test", "v2".into(), binding.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "v2").await;
        tasks.0.lock().prune(Instant::now() + RETENTION / 2);
        assert!(tasks.get_retained("v2").unwrap().result_available);
        assert!(tasks.acknowledge("v2", &epoch, "wrong-digest").is_err());
        assert!(
            tasks
                .acknowledge("v2", "wrong-epoch", &binding.submission_sha256)
                .is_err()
        );
        tasks.acknowledge("v2", &epoch, &binding.submission_sha256).unwrap();
        tasks.acknowledge("v2", &epoch, &binding.submission_sha256).unwrap();
        let response = serde_json::to_value(tasks.get_retained("v2").unwrap()).unwrap();
        assert_eq!(response["status"], "completed");
        assert_eq!(response["acknowledged"], true);
        assert_eq!(response["result_available"], false);
        assert!(response.get("result").is_none());
        assert!(tasks.get("v2").is_err());
        tasks
            .submit("test", "v2".into(), binding, || async {
                panic!("acknowledged ID must not replay")
            })
            .unwrap();
    }

    #[tokio::test]
    async fn pre_dispatch_cancel_is_not_started_and_preserves_intent() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        tasks
            .submit("test", "cancel".into(), binding, || async {
                panic!("cancelled before dispatch")
            })
            .unwrap();
        tasks.cancel("cancel").unwrap();
        terminal(&tasks, "cancel").await;
        let record = tasks.get_retained("cancel").unwrap();
        assert_eq!(record.execution.disposition, Disposition::NotStarted);
        assert!(record.cancellation_requested);
        assert!(!record.result_available);
    }

    #[tokio::test]
    async fn panic_and_runtime_cleanup_uncertainty_never_claim_stopped() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        tasks
            .submit("test", "panic".into(), binding.clone(), || async { panic!("fixture") })
            .unwrap();
        terminal(&tasks, "panic").await;
        assert_eq!(
            tasks.get_retained("panic").unwrap().execution.disposition,
            Disposition::Unknown
        );
        let acknowledged = tasks
            .acknowledge(
                "panic",
                binding.request_epoch.as_deref().unwrap(),
                &binding.submission_sha256,
            )
            .unwrap();
        assert!(acknowledged.acknowledged);
        assert_eq!(acknowledged.execution.disposition, Disposition::Unknown);
        tasks
            .submit("test", "uncertain".into(), binding, || async {
                Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    r#"{"status":"runtime_unavailable"}"#,
                )]))
            })
            .unwrap();
        terminal(&tasks, "uncertain").await;
        assert_eq!(
            tasks.get_retained("uncertain").unwrap().execution.disposition,
            Disposition::Unknown
        );
    }

    #[tokio::test]
    async fn ack_compaction_rotates_only_admission_epoch_and_fences_forgotten_keys() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        for index in 0..MAX_TASKS {
            tasks
                .submit("test", format!("id-{index}"), binding.clone(), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
        }
        terminal(&tasks, "id-0").await;
        tasks
            .acknowledge(
                "id-0",
                binding.request_epoch.as_deref().unwrap(),
                &binding.submission_sha256,
            )
            .unwrap();
        // An exact retry of a still-retained acknowledged key is idempotent,
        // even at capacity; it must not cause its own record to disappear.
        tasks
            .submit("test", "id-0".into(), binding.clone(), || async {
                panic!("acknowledged ID must not replay")
            })
            .unwrap();
        assert_eq!(tasks.status().request_epoch, binding.request_epoch.clone().unwrap());
        // A fresh submission observed the current epoch before admission. The
        // compaction it triggers must admit that same request, not return unknown.
        tasks
            .submit("test", "new".into(), binding.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "new").await;
        assert_ne!(tasks.status().request_epoch, binding.request_epoch.clone().unwrap());
        assert_eq!(
            tasks.get_retained("new").unwrap().execution.disposition,
            Disposition::Stopped
        );
        // It retains its original binding and remains retry-safe across rotation.
        tasks
            .submit("test", "new".into(), binding.clone(), || async {
                panic!("accepted old-epoch request must not replay")
            })
            .unwrap();
        let error = tasks
            .submit("test", "id-0".into(), binding.clone(), || async {
                panic!("forgotten ID must not replay")
            })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "unknown");
        // Other original IDs survive the rotation; a stale absent ID stays fenced.
        assert!(tasks.get_retained("id-1").is_ok());
        assert!(
            tasks
                .submit("test", "stale".into(), binding, || async { panic!("closed epoch") })
                .is_err()
        );
    }

    #[tokio::test]
    async fn journal_retains_result_across_restart_without_relaunching() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .submit("test", "durable".into(), original.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let result = terminal(&tasks, "durable").await;
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        drop(tasks);
        let recovered = RequestTasks::open(dir.path()).unwrap();
        assert_eq!(recovered.get("durable").unwrap(), result);
        assert_ne!(
            recovered.status().request_epoch,
            original.request_epoch.clone().unwrap()
        );
        let mut retry = original.clone();
        retry.server_instance_id = "new-broker".into();
        recovered
            .submit("test", "durable".into(), retry, || async {
                panic!("restart must not replay")
            })
            .unwrap();
        assert_eq!(recovered.get_retained("durable").unwrap().request, Some(original));
    }

    #[test]
    fn journal_recovery_distinguishes_never_dispatched_from_uncertain_and_isolates_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let binding = v2_binding(&tasks);
        for (id, reason) in [("accepted", "accepted"), ("dispatched", "dispatch_intent")] {
            let record = StoredRequest {
                task: DetailedTask::new(Task::new(id, TaskStatus::Working, "now", "now"), TaskPayload::Working),
                request: Some(binding.clone()),
                execution: Execution::new(Disposition::Pending, reason),
                cancellation_requested: false,
                acknowledged: false,
                result_available: false,
                finished_at: None,
            };
            tasks.0.lock().persist(&record).unwrap();
        }
        assert!(RequestTasks::open(dir.path()).is_err(), "second live owner must fail");
        drop(tasks);
        let recovered = RequestTasks::open(dir.path()).unwrap();
        assert_eq!(
            recovered.get_retained("accepted").unwrap().execution.disposition,
            Disposition::NotStarted
        );
        assert_eq!(
            recovered.get_retained("dispatched").unwrap().execution.disposition,
            Disposition::Unknown
        );
        drop(recovered);
        let damaged = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| std::fs::read_to_string(path).is_ok_and(|s| s.contains("dispatched")))
            .unwrap();
        std::fs::write(&damaged, "not json").unwrap();
        let healthy = RequestTasks::open(dir.path()).unwrap();
        assert!(healthy.get_retained("accepted").is_ok());
        assert!(healthy.get_retained("dispatched").is_err());
        assert!(
            healthy
                .submit("test", "dispatched".into(), binding, || async {
                    panic!("damaged request replay")
                })
                .is_err()
        );
        assert!(damaged.with_extension("json.quarantined").exists());
    }

    #[tokio::test]
    async fn full_registry_refusal_is_typed_no_start_not_unknown() {
        let tasks = RequestTasks::default();
        for index in 0..MAX_TASKS {
            tasks
                .submit("test", format!("id-{index}"), binding(1), || async {
                    std::future::pending().await
                })
                .unwrap();
        }
        let error = tasks
            .submit("test", "full".into(), binding(1), || async { panic!("not admitted") })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "not_started");
    }

    #[tokio::test]
    async fn oversized_result_has_no_verdict_but_keeps_execution_disposition() {
        let tasks = RequestTasks::default();
        tasks
            .submit("test", "large".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    "x".repeat(MAX_RECORD_BYTES),
                )]))
            })
            .unwrap();
        terminal(&tasks, "large").await;
        let record = tasks.get_retained("large").unwrap();
        assert_eq!(record.execution.disposition, Disposition::Stopped);
        assert!(!record.result_available);
        assert!(serde_json::to_vec(&record).unwrap().len() < MAX_RECORD_BYTES);
    }
    #[tokio::test]
    async fn uncertainty_survives_successful_tool_fallback() {
        let tasks = RequestTasks::default();
        tasks
            .submit("test", "fallback".into(), v2_binding(&tasks), || async {
                crate::cancellation::CallCancellation::current().mark_uncertain();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "fallback").await;
        let record = tasks.get_retained("fallback").unwrap();
        assert!(record.result_available);
        assert_eq!(record.execution.disposition, Disposition::Unknown);
    }

    #[tokio::test]
    async fn semantic_runtime_error_is_unknown_in_every_carrier() {
        for result in [
            CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                r#"{"data":null,"errors":[{"code":"runtime_unavailable"}]}"#,
            )]),
            CallToolResult::structured(serde_json::json!({"data":null,"errors":[{"code":"runtime_unavailable"}]})),
        ] {
            assert_eq!(
                completion_execution(&Ok(result), false).disposition,
                Disposition::Unknown
            );
        }
    }

    #[tokio::test]
    async fn wrong_epoch_cancel_cannot_signal_an_unrelated_request() {
        let tasks = RequestTasks::default();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let binding = v2_binding(&tasks);
        tasks
            .submit("test", "scoped".into(), binding.clone(), || async {
                finish_rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        assert!(tasks.cancel_scoped("scoped", Some("other-epoch")).is_err());
        assert!(tasks.get_retained_scoped("scoped", Some("other-epoch")).is_err());
        assert!(!tasks.get_retained("scoped").unwrap().cancellation_requested);
        assert!(
            tasks
                .acknowledge(
                    "scoped",
                    binding.request_epoch.as_deref().unwrap(),
                    &binding.submission_sha256
                )
                .is_err()
        );
        finish_tx.send(()).unwrap();
        terminal(&tasks, "scoped").await;
    }

    #[tokio::test]
    async fn journal_write_failure_prevents_execution_and_further_admission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let tasks = RequestTasks::open(&path).unwrap();
        let binding = v2_binding(&tasks);
        std::fs::rename(&path, dir.path().join("moved")).unwrap();
        let error = tasks
            .submit("test", "failed-write".into(), binding.clone(), || async {
                panic!("journal acceptance failed")
            })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "not_started");
        assert!(!tasks.status().durable);
        assert!(!tasks.status().journal_healthy);
        assert!(
            tasks
                .submit("test", "again".into(), binding.clone(), || async {
                    panic!("unhealthy journal")
                })
                .is_err()
        );
        std::fs::rename(dir.path().join("moved"), &path).unwrap();
        tasks
            .submit("test", "recovered".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "recovered").await;
        assert!(tasks.status().journal_healthy);
        let failed = tasks
            .submit("test", "failed-write".into(), binding, || async {
                panic!("failed acceptance ID must not execute on retry")
            })
            .unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn legacy_pre_dispatch_cancellation_still_expires() {
        let tasks = RequestTasks::default();
        let task = tasks
            .spawn("test", || async { panic!("cancelled before start") })
            .unwrap();
        tasks.cancel(&task.task_id).unwrap();
        terminal(&tasks, &task.task_id).await;
        tasks.0.lock().prune(Instant::now() + RETENTION * 2);
        assert!(tasks.get(&task.task_id).is_err());
    }
    #[tokio::test]
    async fn pressure_eviction_fences_old_ids_and_preserves_active_work() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let active_binding = v2_binding(&tasks);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tasks
            .submit("test", "active".into(), active_binding.clone(), || async {
                rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let original = v2_binding(&tasks);
        for i in 0..(MAX_TASKS + 3) {
            tasks
                .submit("test", format!("done-{i}"), v2_binding(&tasks), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
            terminal(&tasks, &format!("done-{i}")).await;
        }
        assert_eq!(tasks.status().active, 1);
        assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
        assert!(tasks.get("done-0").is_err());
        assert!(
            tasks
                .submit("test", "done-0".into(), original, || async { panic!("expired retry") })
                .is_err()
        );
        assert!(
            tasks
                .submit("test", "active".into(), active_binding, || async {
                    panic!("active retry")
                })
                .is_ok()
        );
        tx.send(()).unwrap();
        terminal(&tasks, "active").await;
    }

    #[tokio::test]
    async fn finished_timestamp_expires_across_restart_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .submit("test", "old".into(), original.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "old").await;
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        {
            let mut registry = tasks.0.lock();
            let mut stored = registry.entries["old"].stored.clone();
            stored.finished_at = Some(unix_seconds() - RETENTION.as_secs() - 1);
            registry.persist(&stored).unwrap();
        }
        drop(tasks);
        let recovered = RequestTasks::open(dir.path()).unwrap();
        assert!(recovered.get("old").is_err());
        assert!(
            recovered
                .submit("test", "old".into(), original, || async { panic!("expired retry") })
                .is_err()
        );
    }

    #[tokio::test]
    async fn journal_write_failure_recovers_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let tasks = RequestTasks::open(&path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let original = v2_binding(&tasks);
        tasks
            .submit("test", "running".into(), original.clone(), || async {
                rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        tokio::task::yield_now().await;
        std::fs::rename(&path, dir.path().join("moved")).unwrap();
        tx.send(()).unwrap();
        terminal(&tasks, "running").await;
        assert!(!tasks.status().journal_healthy);
        std::fs::rename(dir.path().join("moved"), &path).unwrap();
        tasks
            .submit("test", "new".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "new").await;
        assert!(tasks.status().journal_healthy);
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        drop(tasks);
        let recovered = RequestTasks::open(&path).unwrap();
        assert_eq!(recovered.get("running").unwrap().status(), TaskStatus::Completed);
        assert!(
            recovered
                .submit("test", "running".into(), original, || async { panic!("replay") })
                .is_ok()
        );
    }
    #[tokio::test]
    async fn failed_expiry_removal_keeps_result_until_storage_returns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal");
        let tasks = RequestTasks::open(&path).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .submit("test", "expired".into(), original.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "expired").await;
        // A directory in place of this result simulates a delete failure without
        // permission/root-dependent behavior or filling the user's disk.
        let file = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let saved = file.with_extension("saved");
        std::fs::rename(&file, &saved).unwrap();
        std::fs::create_dir(&file).unwrap();
        tasks.0.lock().prune(Instant::now() + RETENTION * 2);
        assert!(tasks.get("expired").is_ok());
        assert!(!tasks.status().journal_healthy);
        std::fs::remove_dir(&file).unwrap();
        std::fs::rename(&saved, &file).unwrap();
        tasks.0.lock().prune(Instant::now() + RETENTION * 2);
        assert!(tasks.get("expired").is_err());
        assert!(
            tasks
                .submit("test", "expired".into(), original, || async {
                    panic!("expired replay")
                })
                .is_err()
        );
        tasks
            .submit("test", "next".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "next").await;
        assert!(tasks.status().journal_healthy);
    }
    #[tokio::test]
    async fn legacy_results_keep_ten_minute_expiry_without_expiring_v2_results() {
        let tasks = RequestTasks::default();
        tasks
            .submit("test", "v2-kept".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "v2-kept").await;
        for i in 0..MAX_TASKS - 1 {
            let id = format!("legacy-{i}");
            tasks
                .submit("test", id.clone(), binding(1), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
            terminal(&tasks, &id).await;
        }
        tasks
            .0
            .lock()
            .prune(Instant::now() + LEGACY_RETENTION + Duration::from_secs(1));
        assert!(tasks.get("legacy-0").is_err());
        assert!(tasks.get("v2-kept").is_ok());
        assert_eq!(tasks.status().retained_terminal, 1);
        tasks
            .submit("test", "new-v2".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "new-v2").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_record_recovery_cannot_overflow_restart_or_replay_old_id() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .submit("test", "unreadable".into(), original.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "unreadable").await;
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        drop(tasks);
        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json"))
            .unwrap();
        let bytes = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        let missing_target = dir.path().join("restored-snapshot");
        std::os::unix::fs::symlink(&missing_target, &file).unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        for i in 0..MAX_TASKS {
            let id = format!("new-{i}");
            tasks
                .submit("test", id.clone(), v2_binding(&tasks), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
            terminal(&tasks, &id).await;
        }
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        drop(tasks);
        std::fs::write(&missing_target, &bytes).unwrap();
        assert_eq!(std::fs::read(file.with_extension("json.quarantined")).unwrap(), bytes);
        let recovered = RequestTasks::open(dir.path()).unwrap();
        assert_eq!(recovered.status().retained_terminal, MAX_TASKS);
        assert!(recovered.get("unreadable").is_err());
        assert!(
            recovered
                .submit("test", "unreadable".into(), original, || async {
                    panic!("unreadable old request must not replay")
                })
                .is_err()
        );
    }
}
