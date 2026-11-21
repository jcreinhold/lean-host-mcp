//! Bounded retained requests. The optional journal stores evidence, never work to
//! replay. Only preparation creates tickets; submission never recreates missing IDs.

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

use crate::cancellation::{CallExecutionEvidence, with_cancellation_evidence};
use crate::envelope::{TargetCheckNotStarted, WorkerStopReceipt};
use crate::request_journal::RequestJournal;

const MAX_TASKS: usize = 128;
const RETENTION: Duration = Duration::from_hours(24);
const PREPARED_RETENTION: Duration = Duration::from_secs(300);
const LEGACY_RETENTION: Duration = Duration::from_secs(600);
pub(crate) const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestTasks(Arc<Mutex<Registry>>);

#[derive(Debug)]
struct Registry {
    entries: HashMap<String, Entry>,
    epoch: String,
    next_ticket: u64,
    journal: Option<RequestJournal>,
    journal_error: bool,
    lifecycle_watch_running: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            epoch: uuid::Uuid::new_v4().to_string(),
            next_ticket: 0,
            journal: None,
            journal_error: false,
            lifecycle_watch_running: false,
        }
    }
}

#[derive(Debug)]
struct Entry {
    stored: StoredRequest,
    cancellation: CancellationToken,
    created: Instant,
    expires: Option<Instant>,
    pending_lifecycle: Option<CallExecutionEvidence>,
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
    /// Durable evidence that broker-local observers were unresolved. This is
    /// metadata only after restart, never proof of OS process ownership.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_lifecycle_operations: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_stop_receipt: Option<WorkerStopReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_check: Option<TargetCheckNotStarted>,
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
            worker_stop_receipt: None,
            target_check: None,
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
    /// Immutable incarnation carried by a prepared ticket or legacy evidence.
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
                record.execution = if matches!(record.execution.reason.as_str(), "accepted" | "prepared") {
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
                    pending_lifecycle: None,
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

    /// Reserve a bounded, immutable submission identity without executing tool code.
    pub(crate) fn prepare(&self, mut submission: RequestBinding) -> Result<RetainedTask, McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        registry.compact();
        registry.recover_journal();
        if registry.journal_error {
            return Err(no_start(
                "request_journal_unavailable",
                "Request journal unavailable; nothing was prepared",
            ));
        }
        if registry.entries.len() >= MAX_TASKS {
            return Err(no_start(
                "request_registry_full",
                "Request registry full; no work was submitted",
            ));
        }
        let sequence = registry.next_ticket.checked_add(1).ok_or_else(|| {
            no_start(
                "request_ticket_exhausted",
                "Request ticket counter exhausted; no work was submitted",
            )
        })?;
        registry.next_ticket = sequence;
        submission.request_epoch = Some(registry.epoch.clone());
        let id = format!("{}:{sequence}", registry.epoch);
        let stored = registry.prepare_record(id, submission)?;
        drop(registry);
        retained_snapshot(stored)
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
            let prepared = if let Some((id, submission)) = &identity {
                let entry = registry
                    .entries
                    .get(id)
                    .ok_or_else(|| unknown_request("request_ticket_unknown"))?;
                // Origin is evidence, not retry equality, including after restart.
                let mut original = entry.stored.request.clone();
                if let Some(original) = &mut original {
                    original.server_instance_id.clone_from(&submission.server_instance_id);
                }
                if original.as_ref() != Some(submission) {
                    return Err(McpError::invalid_params(
                        "request_id already belongs to a different submission",
                        Some(serde_json::json!({"reason":"request_identity_conflict",
                            "execution":{"disposition":"unknown","reason":"request_identity_conflict"}})),
                    ));
                }
                if entry.stored.execution.disposition != Disposition::Pending
                    || entry.stored.execution.reason != "prepared"
                {
                    return Ok(Some(entry.stored.task.task.clone()));
                }
                Some(entry.stored.clone())
            } else {
                None
            };
            if identity.is_none() {
                registry.compact();
            }
            registry.recover_journal();
            if registry.journal_error {
                return Err(no_start(
                    "request_journal_unavailable",
                    "Request journal unavailable; no work was submitted",
                ));
            }
            if prepared.is_none() && registry.entries.len() >= MAX_TASKS {
                return Ok(None);
            }
            let (task_id, request) = identity.map_or_else(
                || (uuid::Uuid::new_v4().to_string(), None),
                |(id, request)| (id, Some(request)),
            );
            let now = current_timestamp();
            let task = prepared
                .map_or_else(
                    || Task::new(task_id, TaskStatus::Working, &now, &now),
                    |record| record.task.task,
                )
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
                pending_lifecycle_operations: Vec::new(),
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
                        pending_lifecycle: None,
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
                    pending_lifecycle: None,
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
                Ok((result, evidence)) => tasks.finish(&task_id, result, false, evidence),
                Err(error) => tasks.finish(
                    &task_id,
                    Err(McpError::internal_error(format!("task failed: {error}"), None)),
                    true,
                    CallExecutionEvidence::default(),
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
            if entry.stored.execution.reason == "prepared" {
                entry.stored.execution = Execution::new(Disposition::NotStarted, "cancelled_before_dispatch");
                entry.stored.task = DetailedTask::new(entry.stored.task.task.clone(), TaskPayload::Cancelled);
                entry.stored.finished_at = Some(unix_seconds());
                entry.expires = Instant::now().checked_add(RETENTION);
            }
            let record = entry.stored.clone();
            registry
                .persist(&record)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        drop(registry);
        Ok(())
    }

    #[cfg(test)]
    fn acknowledge(&self, id: &str, epoch: &str, digest: &str) -> Result<RetainedTask, McpError> {
        self.acknowledge_if_retained(id, epoch, digest)?
            .ok_or_else(unknown_task)
    }

    pub(crate) fn acknowledge_if_retained(
        &self,
        id: &str,
        epoch: &str,
        digest: &str,
    ) -> Result<Option<RetainedTask>, McpError> {
        let mut registry = self.0.lock();
        registry.prune(Instant::now());
        registry.recover_journal();
        if registry.journal_error {
            return Err(McpError::internal_error("Request journal unavailable", None));
        }
        let Some(entry) = registry.entries.get_mut(id) else {
            return Ok(None);
        };
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
        retained_snapshot(record).map(Some)
    }

    fn finish(
        &self,
        id: &str,
        result: Result<CallToolResult, McpError>,
        panicked: bool,
        mut evidence: CallExecutionEvidence,
    ) {
        let mut registry = self.0.lock();
        let Some(entry) = registry.entries.get_mut(id) else {
            return;
        };
        // A cancellation signal is not a result. In particular it must never
        // overwrite a real proof result returned while cancellation raced it.
        evidence.refresh_deferred();
        entry.stored.pending_lifecycle_operations = evidence
            .deferred
            .iter()
            .map(|observer| observer.operation_id.clone())
            .collect();
        entry.pending_lifecycle = (!evidence.deferred.is_empty()).then(|| evidence.clone());
        entry.stored.execution = execution_with_evidence(&result, panicked, evidence);
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
        drop(registry);
        self.watch_lifecycle();
    }

    fn watch_lifecycle(&self) {
        let mut registry = self.0.lock();
        if registry.lifecycle_watch_running || !registry.entries.values().any(|entry| entry.pending_lifecycle.is_some())
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        registry.lifecycle_watch_running = true;
        let weak = Arc::downgrade(&self.0);
        drop(registry);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(shared) = weak.upgrade() else { return };
                let mut registry = shared.lock();
                registry.refresh_lifecycle();
                let pending = registry.entries.values().any(|entry| entry.pending_lifecycle.is_some());
                if !pending {
                    registry.lifecycle_watch_running = false;
                }
                drop(registry);
                if !pending {
                    return;
                }
            }
        });
    }
}

impl Registry {
    fn refresh_lifecycle(&mut self) {
        let mut changed = Vec::new();
        for entry in self.entries.values_mut() {
            let Some(evidence) = &mut entry.pending_lifecycle else {
                continue;
            };
            evidence.refresh_deferred();
            if !evidence.deferred.is_empty() {
                continue;
            }
            if !evidence.uncertain && evidence.pending == 0 && evidence.stopped > 0 {
                entry.stored.execution =
                    Execution::new(Disposition::Stopped, "worker_operation_stopped_after_retirement");
                entry.stored.execution.worker_stop_receipt = evidence.worker_stop_receipt.clone();
                entry.stored.execution.target_check = evidence.target_check.clone();
                entry.stored.task.task.last_updated_at = current_timestamp();
            }
            entry.pending_lifecycle = None;
            entry.stored.pending_lifecycle_operations.clear();
            changed.push(entry.stored.clone());
        }
        for record in changed {
            if let Err(error) = self.persist(&record) {
                tracing::error!(%error, "could not journal late worker exit; new admission disabled");
            }
        }
    }
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
        // Submission never creates a missing ID. Forgetting records therefore
        // cannot authorize replay and does not invalidate unrelated tickets.
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
        self.refresh_lifecycle();
        let expired_prepared = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.stored.execution.reason == "prepared" && entry.expires.is_some_and(|expiry| expiry <= now)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired_prepared {
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.stored.execution = Execution::new(Disposition::NotStarted, "request_ticket_expired");
                entry.stored.task = failed_task(
                    entry.stored.task.task.clone(),
                    "request_ticket_expired",
                    "Prepared ticket expired; work was not submitted",
                );
                entry.stored.finished_at = Some(unix_seconds());
                entry.expires = now.checked_add(RETENTION);
                entry.stored.task.task.last_updated_at = current_timestamp();
                entry.stored.task.task.ttl_ms = Some(
                    u64::try_from(entry.created.elapsed().saturating_add(RETENTION).as_millis()).unwrap_or(u64::MAX),
                );
                let record = entry.stored.clone();
                if let Err(error) = self.persist(&record) {
                    tracing::warn!(%error, "could not persist expired preparation; admission disabled until recovery");
                }
            }
        }
        let ids = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.pending_lifecycle.is_none()
                    && entry.stored.task.status().is_terminal()
                    && entry.expires.is_some_and(|expiry| expiry <= now)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.forget(ids);
    }

    fn compact(&mut self) {
        if self.entries.len() < MAX_TASKS {
            return;
        }
        // Never discard an unacknowledged result under pressure. Expired pure
        // preparation has no tool result; its ticket remains permanently fenced.
        let id = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.pending_lifecycle.is_none()
                    && entry.stored.task.status().is_terminal()
                    && (entry.stored.acknowledged
                        || matches!(
                            entry.stored.execution.reason.as_str(),
                            "request_ticket_expired" | "request_preparation_unavailable"
                        ))
            })
            .min_by_key(|(_, entry)| (entry.stored.finished_at, entry.created))
            .map(|(id, _)| id.clone());
        self.forget(id.into_iter().collect());
    }

    fn prepare_record(&mut self, id: String, submission: RequestBinding) -> Result<StoredRequest, McpError> {
        let now = current_timestamp();
        let task = Task::new(id.clone(), TaskStatus::Working, &now, &now)
            .with_status_message("Prepared; awaiting submission")
            .with_ttl_ms(u64::try_from(PREPARED_RETENTION.as_millis()).unwrap_or(u64::MAX))
            .with_poll_interval_ms(2_000);
        let mut stored = StoredRequest {
            task: DetailedTask::new(task, TaskPayload::Working),
            request: Some(submission),
            execution: Execution::new(Disposition::Pending, "prepared"),
            cancellation_requested: false,
            acknowledged: false,
            result_available: false,
            finished_at: None,
            pending_lifecycle_operations: Vec::new(),
        };
        // A failed persist never dispatches. Consume the sequence even on error:
        // atomic rename may already have stored this never-submitted ticket.
        if self.persist(&stored).is_err() {
            // Rename may have succeeded. Account for this ID in memory too;
            // recovery must not accumulate uncounted orphan snapshots.
            stored.execution = Execution::new(Disposition::NotStarted, "request_preparation_unavailable");
            stored.task = failed_task(
                stored.task.task,
                "request_journal_unavailable",
                "Could not persist preparation; no work was submitted",
            );
            stored.finished_at = Some(unix_seconds());
            self.entries.insert(
                id,
                Entry {
                    stored,
                    pending_lifecycle: None,
                    cancellation: CancellationToken::new(),
                    created: Instant::now(),
                    expires: Instant::now().checked_add(RETENTION),
                },
            );
            return Err(no_start(
                "request_journal_unavailable",
                "Could not persist preparation; no work was submitted",
            ));
        }
        self.entries.insert(
            id,
            Entry {
                stored: stored.clone(),
                pending_lifecycle: None,
                cancellation: CancellationToken::new(),
                created: Instant::now(),
                expires: Instant::now().checked_add(PREPARED_RETENTION),
            },
        );
        Ok(stored)
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

fn execution_with_evidence(
    result: &Result<CallToolResult, McpError>,
    panicked: bool,
    evidence: CallExecutionEvidence,
) -> Execution {
    let mut execution = if panicked {
        Execution::new(Disposition::Unknown, "task_panicked")
    } else if evidence.uncertain || evidence.pending > 0 || !evidence.deferred.is_empty() {
        Execution::new(Disposition::Unknown, "worker_operation_failed_without_exit_witness")
    } else if evidence.stopped > 0 {
        Execution::new(Disposition::Stopped, "worker_operation_stopped")
    } else if evidence.returned > 0 {
        Execution::new(Disposition::Stopped, "worker_operations_returned")
    } else if evidence.not_started > 0 {
        Execution::new(Disposition::NotStarted, "worker_operations_not_started")
    } else {
        completion_execution(result, false)
    };
    execution.worker_stop_receipt = evidence.worker_stop_receipt;
    execution.target_check = evidence.target_check;
    execution
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

    #[tokio::test]
    async fn late_reap_after_ack_preserves_request_and_refreshes_only_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(directory.path()).unwrap();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        let binding = prepared.request.unwrap();
        let observed = Arc::new(Mutex::new(None));
        let source = Arc::clone(&observed);
        tasks
            .submit("test", id.clone(), binding.clone(), move || async move {
                let cancellation = crate::cancellation::CallCancellation::current();
                let operation = cancellation.begin_operation_for("late-op".to_owned());
                cancellation.defer_stop("late-op".to_owned(), Arc::new(move || source.lock().clone()));
                operation.unknown();
                Err(McpError::internal_error("proof result unavailable", None))
            })
            .unwrap();
        terminal(&tasks, &id).await;
        assert_eq!(
            tasks.get_retained(&id).unwrap().execution.disposition,
            Disposition::Unknown
        );
        tasks
            .acknowledge(
                &id,
                binding.request_epoch.as_deref().unwrap(),
                &binding.submission_sha256,
            )
            .unwrap();
        for _ in 0..MAX_TASKS - 1 {
            tasks.prepare(binding.clone()).unwrap();
        }
        assert!(tasks.prepare(binding.clone()).is_err());
        assert!(tasks.get_retained(&id).unwrap().acknowledged);
        tasks.0.lock().prune(Instant::now() + RETENTION * 2);
        assert!(
            tasks.get_retained(&id).is_ok(),
            "TTL must preserve an unresolved lifecycle"
        );
        *observed.lock() = Some(WorkerStopReceipt {
            operation_id: "late-op".to_owned(),
            worker_session_id: "session".to_owned(),
            worker_generation: 7,
            worker_scope: "main".to_owned(),
            worker_request_id: Some(1),
            worker_pid: Some(123),
            exit_success: Some(false),
            exit_code: None,
            exit_status: Some("killed".to_owned()),
            operation: "check".to_owned(),
            cause: "timeout".to_owned(),
        });
        let retained = tasks.get_retained(&id).unwrap();
        assert_eq!(retained.execution.disposition, Disposition::Stopped);
        assert_eq!(retained.execution.worker_stop_receipt.unwrap().operation_id, "late-op");
        assert!(retained.acknowledged && !retained.result_available);
        let stored = tasks.0.lock().entries[&id].stored.clone();
        assert!(stored.task.status().is_terminal());
        assert!(tasks.0.lock().entries[&id].pending_lifecycle.is_none());
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    std::fs::read_to_string(entry.path())
                        .is_ok_and(|text| text.contains("worker_operation_stopped_after_retirement"))
                })
        );
    }

    #[tokio::test]
    async fn closed_scope_without_wait_status_clears_retention_hold_but_preserves_unknown() {
        use crate::cancellation::DeferredStopState;
        let directory = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(directory.path()).unwrap();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        let binding = prepared.request.unwrap();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source = Arc::clone(&closed);
        tasks
            .submit("test", id.clone(), binding.clone(), move || async move {
                let cancellation = crate::cancellation::CallCancellation::current();
                let operation = cancellation.begin_operation_for("lost-wait".to_owned());
                cancellation.defer_stop_state(
                    "lost-wait".to_owned(),
                    Arc::new(move || {
                        if source.load(std::sync::atomic::Ordering::Acquire) {
                            DeferredStopState::ScopeClosedWithoutStatus
                        } else {
                            DeferredStopState::Pending
                        }
                    }),
                );
                operation.unknown();
                Err(McpError::internal_error("proof result unavailable", None))
            })
            .unwrap();
        terminal(&tasks, &id).await;
        assert!(tasks.0.lock().entries[&id].pending_lifecycle.is_some());
        tasks
            .acknowledge(
                &id,
                binding.request_epoch.as_deref().unwrap(),
                &binding.submission_sha256,
            )
            .unwrap();
        closed.store(true, std::sync::atomic::Ordering::Release);
        let retained = tasks.get_retained(&id).unwrap();
        assert_eq!(retained.execution.disposition, Disposition::Unknown);
        assert!(retained.execution.worker_stop_receipt.is_none());
        assert!(retained.acknowledged && !retained.result_available);
        let mut registry = tasks.0.lock();
        assert!(registry.entries[&id].pending_lifecycle.is_none());
        assert!(registry.entries[&id].stored.pending_lifecycle_operations.is_empty());
        registry.prune(Instant::now() + RETENTION * 2);
        assert!(!registry.entries.contains_key(&id));
        drop(registry);
    }

    #[test]
    fn pending_journal_metadata_does_not_fabricate_post_restart_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(directory.path()).unwrap();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        {
            let mut registry = tasks.0.lock();
            let entry = registry.entries.get_mut(&id).unwrap();
            entry.stored.execution = Execution::new(Disposition::Unknown, "cleanup_unconfirmed");
            entry.stored.pending_lifecycle_operations = vec!["operation-with-lost-owner".to_owned()];
            entry.stored.task = failed_task(entry.stored.task.task.clone(), "cleanup_unconfirmed", "no proof result");
            entry.stored.finished_at = Some(unix_seconds());
            let record = entry.stored.clone();
            registry.persist(&record).unwrap();
        }
        drop(tasks);
        let recovered = RequestTasks::open(directory.path()).unwrap();
        let registry = recovered.0.lock();
        let entry = &registry.entries[&id];
        assert_eq!(entry.stored.pending_lifecycle_operations, ["operation-with-lost-owner"]);
        assert_eq!(entry.stored.execution.disposition, Disposition::Unknown);
        assert!(entry.pending_lifecycle.is_none());
        drop(registry);
    }

    // Named prepared fixtures keep legacy journal/binding assertions readable.
    // Replay and v3 identity invariants use the real prepare/submit API in
    // admission_tests; this helper never replaces an existing record.
    impl RequestTasks {
        fn prepare_submit<F, Fut>(
            &self,
            tool: &str,
            id: String,
            submission: RequestBinding,
            operation: F,
        ) -> Result<Task, McpError>
        where
            F: FnOnce() -> Fut + Send + 'static,
            Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
        {
            {
                let mut registry = self.0.lock();
                if !registry.entries.contains_key(&id) {
                    if submission
                        .request_epoch
                        .as_ref()
                        .is_some_and(|epoch| epoch != &registry.epoch)
                    {
                        return Err(unknown_request("request_ticket_unknown"));
                    }
                    registry.prune(Instant::now());
                    registry.compact();
                    registry.recover_journal();
                    if registry.journal_error {
                        return Err(no_start("request_journal_unavailable", "Journal unavailable"));
                    }
                    if registry.entries.len() >= MAX_TASKS {
                        return Err(no_start("request_registry_full", "Registry full"));
                    }
                    registry.prepare_record(id.clone(), submission.clone())?;
                }
            }
            self.submit(tool, id, submission, operation)
        }
    }

    fn stop_receipt() -> WorkerStopReceipt {
        WorkerStopReceipt {
            operation_id: "operation-1".to_owned(),
            worker_session_id: "worker-session-1".to_owned(),
            worker_generation: 7,
            worker_scope: "main".to_owned(),
            worker_request_id: Some(1),
            worker_pid: Some(123),
            exit_success: Some(false),
            exit_code: None,
            exit_status: Some("killed".to_owned()),
            operation: "module_query".to_owned(),
            cause: "timeout".to_owned(),
        }
    }

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
            .prepare_submit("test", "client-chosen-id".into(), binding(1), move || async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let retry = tasks
            .prepare_submit("test", "client-chosen-id".into(), binding(1), || async {
                panic!("retry must not execute")
            })
            .unwrap();
        assert_eq!(original.task_id, retry.task_id);
        let completed = terminal(&tasks, &original.task_id).await;
        assert_eq!(completed.status(), TaskStatus::Completed);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            tasks
                .prepare_submit("test", "client-chosen-id".into(), binding(2), || async {
                    panic!("conflict must not execute")
                })
                .is_err()
        );
        let retry = tasks
            .prepare_submit("test", "client-chosen-id".into(), binding(1), || async {
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
                .prepare_submit("test", format!("id-{index}"), binding(1), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
        }
        assert!(
            tasks
                .prepare_submit("test", "overflow".into(), binding(1), || async {
                    panic!("overflow must not execute")
                })
                .is_err()
        );
        assert!(
            tasks
                .prepare_submit("test", "id-0".into(), binding(1), || async {
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
            .prepare_submit("test", "working-fixture".into(), binding(1), || async {
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
            .prepare_submit("test", "failed-fixture".into(), binding(1), || async {
                Err(McpError::invalid_params("test failure", None))
            })
            .unwrap();
        terminal(&tasks, &failed.task_id).await;
        observe(&failed.task_id, "failed");
        let cancelled = tasks
            .prepare_submit("test", "cancelled-fixture".into(), binding(1), || async {
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
            .prepare_submit("test", "v2".into(), binding.clone(), || async {
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
            .prepare_submit("test", "v2".into(), binding, || async {
                panic!("acknowledged ID must not replay")
            })
            .unwrap();
    }

    #[tokio::test]
    async fn pre_dispatch_cancel_is_not_started_and_preserves_intent() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        tasks
            .prepare_submit("test", "cancel".into(), binding, || async {
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
            .prepare_submit("test", "panic".into(), binding.clone(), || async { panic!("fixture") })
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
            .prepare_submit("test", "uncertain".into(), binding, || async {
                Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    r#"{"status":"runtime_unavailable"}"#,
                )]))
            })
            .unwrap();
        terminal(&tasks, "uncertain").await;
        let uncertain = tasks.get_retained("uncertain").unwrap();
        assert_eq!(uncertain.execution.disposition, Disposition::Unknown);
        assert!(uncertain.execution.target_check.is_none());
    }

    #[tokio::test]
    async fn ack_compaction_preserves_admission_identity_and_fences_forgotten_keys() {
        let tasks = RequestTasks::default();
        let binding = v2_binding(&tasks);
        for index in 0..MAX_TASKS {
            tasks
                .prepare_submit("test", format!("id-{index}"), binding.clone(), || async {
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
            .prepare_submit("test", "id-0".into(), binding.clone(), || async {
                panic!("acknowledged ID must not replay")
            })
            .unwrap();
        assert_eq!(tasks.status().request_epoch, binding.request_epoch.clone().unwrap());
        // A fresh submission observed the current epoch before admission. The
        // compaction it triggers must admit that same request, not return unknown.
        tasks
            .prepare_submit("test", "new".into(), binding.clone(), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "new").await;
        assert_eq!(tasks.status().request_epoch, binding.request_epoch.clone().unwrap());
        assert_eq!(
            tasks.get_retained("new").unwrap().execution.disposition,
            Disposition::Stopped
        );
        // Its original binding remains retry-safe without global rotation.
        tasks
            .prepare_submit("test", "new".into(), binding.clone(), || async {
                panic!("accepted old-epoch request must not replay")
            })
            .unwrap();
        let error = tasks
            .submit("test", "id-0".into(), binding.clone(), || async {
                panic!("forgotten ID must not replay")
            })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "unknown");
        // Other original IDs survive compaction; every absent ticket stays fenced.
        assert!(tasks.get_retained("id-1").is_ok());
        assert!(
            tasks
                .submit("test", "stale".into(), binding, || async { panic!("absent ticket") })
                .is_err()
        );
    }

    #[tokio::test]
    async fn journal_retains_result_across_restart_without_relaunching() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .prepare_submit("test", "durable".into(), original.clone(), || async {
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
            .prepare_submit("test", "durable".into(), retry, || async {
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
                pending_lifecycle_operations: Vec::new(),
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
                .prepare_submit("test", "dispatched".into(), binding, || async {
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
                .prepare_submit("test", format!("id-{index}"), binding(1), || async {
                    std::future::pending().await
                })
                .unwrap();
        }
        let error = tasks
            .prepare_submit("test", "full".into(), binding(1), || async { panic!("not admitted") })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "not_started");
    }

    #[tokio::test]
    async fn oversized_result_has_no_verdict_but_keeps_execution_disposition() {
        let tasks = RequestTasks::default();
        tasks
            .prepare_submit("test", "large".into(), v2_binding(&tasks), || async {
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
            .prepare_submit("test", "fallback".into(), v2_binding(&tasks), || async {
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
    async fn stopped_worker_error_keeps_its_original_result_in_every_carrier() {
        let value = serde_json::json!({"data":null,"errors":[{"code":"runtime_unavailable"}]});
        let text = rmcp::model::ContentBlock::text(value.to_string());
        let mut both = CallToolResult::structured(value.clone());
        both.content.push(text.clone());
        for (index, result) in [
            CallToolResult::success(vec![text]),
            CallToolResult::structured(value),
            both,
        ]
        .into_iter()
        .enumerate()
        {
            let expected = serde_json::to_value(&result).unwrap();
            let tasks = RequestTasks::default();
            let id = format!("stopped-{index}");
            tasks
                .prepare_submit("test", id.clone(), v2_binding(&tasks), || async {
                    crate::cancellation::CallCancellation::current()
                        .begin_operation()
                        .stopped(stop_receipt());
                    Ok(result)
                })
                .unwrap();
            terminal(&tasks, &id).await;
            let record = tasks.get_retained(&id).unwrap();
            assert_eq!(record.execution.disposition, Disposition::Stopped);
            assert_eq!(record.execution.worker_stop_receipt, Some(stop_receipt()));
            assert_eq!(serde_json::to_value(record).unwrap()["result"], expected);
        }
    }

    #[tokio::test]
    async fn successful_fallback_cannot_settle_an_earlier_unknown_operation() {
        let tasks = RequestTasks::default();
        tasks
            .prepare_submit("test", "mixed-fallback".into(), v2_binding(&tasks), || async {
                let cancellation = crate::cancellation::CallCancellation::current();
                cancellation.begin_operation().unknown();
                cancellation.begin_operation().stopped(stop_receipt());
                cancellation.begin_operation().returned();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "mixed-fallback").await;
        let record = tasks.get_retained("mixed-fallback").unwrap();
        assert_eq!(record.execution.disposition, Disposition::Unknown);
        assert_eq!(record.execution.worker_stop_receipt, Some(stop_receipt()));
    }

    #[test]
    fn later_queue_refusal_does_not_hide_earlier_completed_work() {
        let result = Err(no_start("queue_wait_timeout", "No later operation was dispatched"));
        let execution = execution_with_evidence(
            &result,
            false,
            CallExecutionEvidence {
                returned: 1,
                not_started: 1,
                ..CallExecutionEvidence::default()
            },
        );
        assert_eq!(execution.disposition, Disposition::Stopped);
        let never_started = execution_with_evidence(
            &result,
            false,
            CallExecutionEvidence {
                not_started: 1,
                ..CallExecutionEvidence::default()
            },
        );
        assert_eq!(never_started.disposition, Disposition::NotStarted);
    }

    #[test]
    fn pending_operation_or_panic_cannot_be_settled_by_a_stop_receipt() {
        for (panicked, pending) in [(false, 1), (true, 0)] {
            let execution = execution_with_evidence(
                &Ok(CallToolResult::success(vec![])),
                panicked,
                CallExecutionEvidence {
                    pending,
                    stopped: 1,
                    worker_stop_receipt: Some(stop_receipt()),
                    ..CallExecutionEvidence::default()
                },
            );
            assert_eq!(execution.disposition, Disposition::Unknown);
        }
    }

    #[tokio::test]
    async fn stopped_error_receipt_survives_restart_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .prepare_submit("test", "stopped-durable".into(), original.clone(), || async {
                crate::cancellation::CallCancellation::current()
                    .begin_operation()
                    .stopped(stop_receipt());
                Err(McpError::internal_error("worker timed out; no proof result", None))
            })
            .unwrap();
        terminal(&tasks, "stopped-durable").await;
        while Arc::strong_count(&tasks.0) > 1 {
            tokio::task::yield_now().await;
        }
        drop(tasks);
        let recovered = RequestTasks::open(dir.path()).unwrap();
        recovered
            .prepare_submit("test", "stopped-durable".into(), original.clone(), || async {
                panic!("a stopped error must not replay")
            })
            .unwrap();
        let record = recovered.get_retained("stopped-durable").unwrap();
        assert_eq!(record.execution.disposition, Disposition::Stopped);
        assert_eq!(record.execution.worker_stop_receipt, Some(stop_receipt()));
        assert_eq!(record.request, Some(original));
        assert_eq!(record.fields["status"], "failed");
        assert!(record.fields.get("result").is_none());
    }

    /// A private warm controller keeps genuine SH custody while writer demand
    /// excludes new readers. No Lean process, installed helper, or live registry
    /// participates; constructing outside Tokio disables automatic retirement.
    struct AdmissionExpiryFixture {
        _directory: tempfile::TempDir,
        root: std::path::PathBuf,
        broker: Arc<crate::broker::ProjectBroker>,
        _demand: lean_toolchain::writer_demand::WriterDemand,
    }

    impl AdmissionExpiryFixture {
        fn new(timeout_millis: u64) -> Self {
            use std::fs::{self, OpenOptions};
            use std::num::NonZeroUsize;

            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap().join("Project");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("lakefile.lean"), "package Project\nlean_lib Project\n").unwrap();
            fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
            fs::write(root.join("lake-manifest.json"), "{\"packages\":[]}\n").unwrap();
            let runtime = crate::project::ProjectRuntimeConfig::from_env_with_file(
                &crate::config_file::RuntimeFileConfig {
                    request_timeout_millis: Some(timeout_millis),
                    ..Default::default()
                },
                1,
            )
            .unwrap()
            .with_operator_primary(Some(&root))
            .expect("approve the private admission fixture");
            assert_eq!(runtime.request_timeout_millis(), timeout_millis);
            let broker = crate::broker::ProjectBroker::new_with_runtime_config(
                crate::broker::BrokerConfig {
                    config_default: None,
                    env_default: None,
                    cwd: root.clone(),
                    max_projects: NonZeroUsize::MIN,
                    max_workers_per_project: NonZeroUsize::MIN,
                    idle_timeout: Duration::ZERO,
                },
                runtime,
            );
            let lock = lean_toolchain::shared_writer::project_lock_path(&root).unwrap();
            let shared = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock)
                .unwrap();
            shared.lock_shared().unwrap();
            broker.test_register_actor(crate::project::LeanProject::test_handoff_actor(
                &root,
                crate::lake_meta::LakeProjectMeta::from_explicit(&root)
                    .unwrap()
                    .manifest_hash,
                vec![root.clone()],
                Box::new(shared),
            ));
            let demand =
                lean_toolchain::writer_demand::WriterDemand::announce(&[lock], Duration::from_secs(1), &|| false)
                    .unwrap();
            Self {
                _directory: directory,
                root,
                broker,
                _demand: demand,
            }
        }
    }

    async fn verify_after_admission(
        broker: Arc<crate::broker::ProjectBroker>,
        root: std::path::PathBuf,
    ) -> Result<CallToolResult, McpError> {
        use lean_rs_worker_parent::{
            LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationTarget, LeanWorkerElabOptions,
            LeanWorkerOutputBudgets, LeanWorkerSorryPolicy,
        };

        broker
            .verify_declaration(
                crate::broker::ProjectHint::Explicit(root),
                vec!["Init".to_owned()],
                vec!["Init".to_owned()],
                LeanWorkerDeclarationVerificationRequest {
                    source: "theorem pending_target : True := True.intro".to_owned(),
                    target: LeanWorkerDeclarationVerificationTarget::Name {
                        name: "pending_target".to_owned(),
                    },
                    sorry_policy: LeanWorkerSorryPolicy::Deny,
                    report_axioms: true,
                    budgets: LeanWorkerOutputBudgets::default(),
                },
                LeanWorkerElabOptions::new().file_label("AdmissionTarget.lean"),
            )
            .await
            .map(|_| CallToolResult::success(vec![]))
            .map_err(McpError::from)
    }

    #[test]
    fn retained_admission_expiry_scopes_non_dispatch_without_erasing_preparation() {
        for preparation in ["none", "returned", "uncertain"] {
            let fixture = AdmissionExpiryFixture::new(40);
            let journal = fixture.root.join("requests");
            let effect = fixture.root.join("preparation-effect");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let tasks = RequestTasks::open(&journal).unwrap();
                let prepared = tasks.prepare(binding(1)).unwrap();
                let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
                let original = prepared.request.unwrap();
                let broker = Arc::clone(&fixture.broker);
                let root = fixture.root.clone();
                let effect_in_call = effect.clone();
                tasks
                    .submit("test", id.clone(), original.clone(), move || async move {
                        if preparation != "none" {
                            crate::lake_options::successful_preparation_for_test(&effect_in_call)
                                .await
                                .map_err(McpError::from)?;
                        }
                        if preparation == "uncertain" {
                            crate::cancellation::CallCancellation::current()
                                .begin_operation()
                                .unknown();
                        }
                        let result = verify_after_admission(broker, root).await;
                        if preparation == "returned" {
                            let error = result.unwrap_err();
                            Ok(CallToolResult::structured(serde_json::json!({
                                "status": "runtime_unavailable", "result": null, "runtime_error": error.data
                            })))
                        } else {
                            result
                        }
                    })
                    .unwrap();
                assert_eq!(
                    terminal(&tasks, &id).await.status(),
                    if preparation == "returned" {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Failed
                    }
                );
                let retained = tasks.get_retained(&id).unwrap();
                assert_eq!(retained.request, Some(original.clone()));
                assert!(retained.result_available && !retained.acknowledged);
                if preparation == "returned" {
                    let envelope = &retained.fields["result"]["structuredContent"];
                    assert_eq!(envelope["status"], "runtime_unavailable");
                    assert_eq!(envelope["runtime_error"]["reason"], "project_admission_timeout");
                    assert!(envelope["result"].is_null(), "no proof verdict exists");
                } else {
                    assert_eq!(retained.fields["error"]["data"]["reason"], "project_admission_timeout");
                    assert!(retained.fields.get("result").is_none(), "no proof verdict exists");
                }
                assert_eq!(
                    retained.execution.disposition,
                    if preparation == "returned" {
                        Disposition::Stopped
                    } else {
                        Disposition::Unknown
                    }
                );
                assert_eq!(
                    retained.execution.target_check,
                    Some(TargetCheckNotStarted {
                        file_label: "AdmissionTarget.lean".to_owned(),
                        stage: "project_admission".to_owned(),
                        disposition: "not_started".to_owned(),
                    })
                );
                assert!(retained.execution.worker_stop_receipt.is_none());
                assert_eq!(effect.exists(), preparation != "none");
                if effect.exists() {
                    assert_eq!(std::fs::read_to_string(&effect).unwrap(), "prepared");
                }
                assert_eq!(
                    fixture
                        .broker
                        .runtime_control(&crate::broker::RuntimeControlRequest::default())
                        .pool
                        .outstanding_jobs,
                    0
                );
                let saved = serde_json::to_value(retained).unwrap();
                tasks
                    .submit("test", id.clone(), original.clone(), || async {
                        panic!("retained admission result must never dispatch again")
                    })
                    .unwrap();
                assert_eq!(serde_json::to_value(tasks.get_retained(&id).unwrap()).unwrap(), saved);
                tokio::time::timeout(Duration::from_secs(2), async {
                    while Arc::strong_count(&tasks.0) > 1 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                drop(tasks);
                let reopened = RequestTasks::open(&journal).unwrap();
                reopened
                    .submit("test", id.clone(), original, || async {
                        panic!("journal recovery must never replay the named check")
                    })
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(reopened.get_retained(&id).unwrap()).unwrap(),
                    saved
                );
            });
            fixture.broker.shutdown_all();
        }
    }

    #[test]
    fn retained_cancellation_during_admission_certifies_only_the_named_check() {
        let fixture = AdmissionExpiryFixture::new(1_000);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let tasks = RequestTasks::default();
            let prepared = tasks.prepare(binding(1)).unwrap();
            let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
            let original = prepared.request.unwrap();
            let broker = Arc::clone(&fixture.broker);
            let root = fixture.root.clone();
            let (entered, waiting) = tokio::sync::oneshot::channel();
            tasks
                .submit("test", id.clone(), original.clone(), move || async move {
                    entered.send(()).unwrap();
                    verify_after_admission(broker, root).await
                })
                .unwrap();
            waiting.await.unwrap();
            assert_eq!(tasks.get(&id).unwrap().status(), TaskStatus::Working);
            tasks.cancel(&id).unwrap();
            assert_eq!(terminal(&tasks, &id).await.status(), TaskStatus::Failed);
            let retained = tasks.get_retained(&id).unwrap();
            assert_eq!(retained.request, Some(original));
            assert!(retained.cancellation_requested);
            assert_eq!(retained.execution.disposition, Disposition::Unknown);
            assert_eq!(retained.fields["error"]["data"]["reason"], "request_cancelled");
            let target = retained.execution.target_check.unwrap();
            assert_eq!(target.file_label, "AdmissionTarget.lean");
            assert_eq!(target.stage, "project_admission");
            assert_eq!(target.disposition, "not_started");
            assert!(retained.fields.get("result").is_none());
            assert!(retained.execution.worker_stop_receipt.is_none());
        });
        fixture.broker.shutdown_all();
    }

    #[tokio::test]
    async fn retained_runtime_unavailable_history_keeps_unknown_without_dispatch_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(directory.path()).unwrap();
        let original = v2_binding(&tasks);
        tasks
            .prepare_submit("test", "saved-unknown-admission".into(), original.clone(), || async {
                Ok(CallToolResult::structured(serde_json::json!({
                    "status": "runtime_unavailable", "result": null,
                    "runtime_error": {"reason":"project_admission_timeout", "retryable":true, "worker_generation":0}
                })))
            })
            .unwrap();
        terminal(&tasks, "saved-unknown-admission").await;
        let record = tasks.get_retained("saved-unknown-admission").unwrap();
        assert_eq!(record.execution.disposition, Disposition::Unknown);
        assert!(record.execution.target_check.is_none());
        let saved = serde_json::to_value(record).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&tasks.0) > 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(tasks);
        let reopened = RequestTasks::open(directory.path()).unwrap();
        reopened
            .prepare_submit("test", "saved-unknown-admission".into(), original, || async {
                panic!("historical unknown admission outcome must not be replayed")
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(reopened.get_retained("saved-unknown-admission").unwrap()).unwrap(),
            saved
        );
    }

    #[tokio::test]
    async fn preparation_failure_only_certifies_the_named_target_check_not_started() {
        for earlier_check_returned in [false, true] {
            let tasks = RequestTasks::default();
            tasks
                .prepare_submit(
                    "test",
                    "preparation-failed".into(),
                    v2_binding(&tasks),
                    move || async move {
                        let cancellation = crate::cancellation::CallCancellation::current();
                        if earlier_check_returned {
                            cancellation.begin_operation().returned();
                        }
                        cancellation.begin_operation().unknown();
                        cancellation.target_check_not_started(TargetCheckNotStarted {
                            disposition: "not_started".to_owned(),
                            file_label: "UnitIndexed.lean".to_owned(),
                            stage: "lake_option_discovery".to_owned(),
                        });
                        Err(McpError::internal_error(
                            "Lake option discovery timed out; no target proof check ran",
                            None,
                        ))
                    },
                )
                .unwrap();
            terminal(&tasks, "preparation-failed").await;
            let record = tasks.get_retained("preparation-failed").unwrap();
            assert_eq!(record.execution.disposition, Disposition::Unknown);
            assert!(record.execution.worker_stop_receipt.is_none());
            let value = serde_json::to_value(record).unwrap();
            assert_eq!(value["execution"]["target_check"]["disposition"], "not_started");
            assert_eq!(value["execution"]["target_check"]["file_label"], "UnitIndexed.lean");
            assert_eq!(value["execution"]["target_check"]["stage"], "lake_option_discovery");
        }
    }

    #[tokio::test]
    async fn missing_import_after_replacement_is_stopped_without_a_target_proof_or_replay() {
        let tasks = RequestTasks::default();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        let original = prepared.request.unwrap();
        tasks
            .submit("test", id.clone(), original.clone(), || async {
                crate::project::receipt_tests::retained_missing_import_after_replacement_call()
                    .await
                    .map(|_| CallToolResult::success(vec![]))
                    .map_err(McpError::from)
            })
            .unwrap();
        terminal(&tasks, &id).await;
        let retained = tasks.get_retained(&id).unwrap();
        assert_eq!(retained.execution.disposition, Disposition::Stopped);
        assert!(
            retained.execution.worker_stop_receipt.is_none(),
            "no old receipt reused"
        );
        let target = retained.execution.target_check.as_ref().unwrap();
        assert_eq!(target.disposition, "not_started");
        assert_eq!(target.file_label, "UnitIndexed.lean");
        assert_eq!(target.stage, "host_session_import");
        assert!(retained.fields.get("result").is_none(), "no proof result invented");
        let saved = serde_json::to_value(&retained).unwrap();
        tasks
            .submit("test", id.clone(), original, || async {
                panic!("never replay missing-build target")
            })
            .unwrap();
        assert_eq!(serde_json::to_value(tasks.get_retained(&id).unwrap()).unwrap(), saved);
    }

    #[tokio::test]
    async fn prepared_request_survives_one_warm_prebody_cycle_without_body_replay() {
        let tasks = RequestTasks::default();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        let original = prepared.request.unwrap();
        tasks
            .submit("test", id.clone(), original.clone(), || async {
                let (outcome, _) = crate::project::receipt_tests::retained_warm_prebody_cycle_call()
                    .await
                    .map_err(McpError::from)?
                    .into_parts();
                Ok(CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    serde_json::to_string(&outcome).unwrap(),
                )]))
            })
            .unwrap();
        assert_eq!(terminal(&tasks, &id).await.status(), TaskStatus::Completed);
        let retained = tasks.get_retained(&id).unwrap();
        assert_eq!(retained.fields["taskId"], id);
        assert_eq!(retained.request.as_ref().unwrap(), &original);
        assert_eq!(retained.execution.disposition, Disposition::Stopped);
        assert!(
            retained.result_available,
            "completed protocol result remains on its original ticket"
        );
        assert!(retained.fields.get("result").is_some());
        assert_eq!(
            tasks.0.lock().entries.len(),
            1,
            "the pre-body cycle creates no outer request"
        );
        let saved = serde_json::to_value(retained).unwrap();
        tasks
            .submit("test", id.clone(), original, || async {
                panic!("a lost response cannot redispatch the completed target")
            })
            .unwrap();
        assert_eq!(serde_json::to_value(tasks.get_retained(&id).unwrap()).unwrap(), saved);
    }

    #[tokio::test]
    async fn prepared_request_keeps_actual_actor_stop_receipt_without_a_proof_result() {
        for failed_replacement in [false, true] {
            let tasks = RequestTasks::default();
            let prepared = tasks.prepare(binding(1)).unwrap();
            let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
            let original = prepared.request.unwrap();
            tasks
                .submit("test", id.clone(), original.clone(), move || async move {
                    crate::project::receipt_tests::retained_timeout_call(failed_replacement)
                        .await
                        .map(|_| CallToolResult::success(vec![]))
                        .map_err(McpError::from)
                })
                .unwrap();
            let result = terminal(&tasks, &id).await;
            assert_eq!(result.status(), TaskStatus::Failed);
            let retained = tasks.get_retained(&id).unwrap();
            assert_eq!(retained.execution.disposition, Disposition::Stopped);
            assert!(
                retained.execution.target_check.is_none(),
                "the actor already dispatched this operation"
            );
            let receipt = retained.execution.worker_stop_receipt.unwrap();
            assert_eq!(receipt.worker_scope, "main");
            assert!(receipt.worker_request_id.is_some());
            assert!(receipt.worker_pid.is_some());
            assert_eq!(receipt.exit_success, Some(false));
            tasks
                .submit("test", id.clone(), original, || async {
                    panic!("a stopped error cannot replay")
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn successful_preparation_then_actor_cancellation_is_stopped_without_a_proof_result() {
        let tasks = RequestTasks::default();
        let root = tempfile::tempdir().unwrap();
        let effect = root.path().join("preparation-effect");
        let effect_in_call = effect.clone();
        let prepared = tasks.prepare(binding(1)).unwrap();
        let id = prepared.fields["taskId"].as_str().unwrap().to_owned();
        tasks
            .submit("test", id.clone(), prepared.request.unwrap(), move || async move {
                crate::lake_options::successful_preparation_for_test(&effect_in_call)
                    .await
                    .map_err(McpError::from)?;
                crate::project::receipt_tests::retained_cancelled_before_dispatch_call()
                    .await
                    .map(|_| CallToolResult::success(vec![]))
                    .map_err(McpError::from)
            })
            .unwrap();
        assert_eq!(terminal(&tasks, &id).await.status(), TaskStatus::Failed);
        assert_eq!(std::fs::read_to_string(effect).unwrap(), "prepared");
        let retained = tasks.get_retained(&id).unwrap();
        assert_eq!(retained.execution.disposition, Disposition::Stopped);
        assert!(
            retained.execution.target_check.is_none(),
            "actor queue cancellation is outside broker admission"
        );
        assert!(retained.execution.worker_stop_receipt.is_none());
    }

    #[tokio::test]
    async fn wrong_epoch_cancel_cannot_signal_an_unrelated_request() {
        let tasks = RequestTasks::default();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let binding = v2_binding(&tasks);
        tasks
            .prepare_submit("test", "scoped".into(), binding.clone(), || async {
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
            .prepare_submit("test", "failed-write".into(), binding.clone(), || async {
                panic!("journal acceptance failed")
            })
            .unwrap_err();
        assert_eq!(error.data.unwrap()["execution"]["disposition"], "not_started");
        assert!(!tasks.status().durable);
        assert!(!tasks.status().journal_healthy);
        assert!(
            tasks
                .prepare_submit("test", "again".into(), binding.clone(), || async {
                    panic!("unhealthy journal")
                })
                .is_err()
        );
        std::fs::rename(dir.path().join("moved"), &path).unwrap();
        tasks
            .prepare_submit("test", "recovered".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "recovered").await;
        assert!(tasks.status().journal_healthy);
        let failed = tasks
            .prepare_submit("test", "failed-write".into(), binding, || async {
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
    async fn pressure_backpressure_preserves_unacknowledged_results_and_active_work() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = RequestTasks::open(dir.path()).unwrap();
        let active_binding = v2_binding(&tasks);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tasks
            .prepare_submit("test", "active".into(), active_binding.clone(), || async {
                rx.await.unwrap();
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        let original = v2_binding(&tasks);
        for i in 0..(MAX_TASKS - 1) {
            tasks
                .prepare_submit("test", format!("done-{i}"), v2_binding(&tasks), || async {
                    Ok(CallToolResult::success(vec![]))
                })
                .unwrap();
            terminal(&tasks, &format!("done-{i}")).await;
        }
        assert_eq!(tasks.status().active, 1);
        assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
        assert!(tasks.get("done-0").is_ok());
        assert!(
            tasks
                .submit("test", "done-0".into(), original, || async { panic!("retained retry") })
                .is_ok()
        );
        assert!(
            tasks
                .prepare_submit("test", "active".into(), active_binding, || async {
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
            .prepare_submit("test", "old".into(), original.clone(), || async {
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
                .prepare_submit("test", "old".into(), original, || async { panic!("expired retry") })
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
            .prepare_submit("test", "running".into(), original.clone(), || async {
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
            .prepare_submit("test", "new".into(), v2_binding(&tasks), || async {
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
                .prepare_submit("test", "running".into(), original, || async { panic!("replay") })
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
            .prepare_submit("test", "expired".into(), original.clone(), || async {
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
            .prepare_submit("test", "next".into(), v2_binding(&tasks), || async {
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
            .prepare_submit("test", "v2-kept".into(), v2_binding(&tasks), || async {
                Ok(CallToolResult::success(vec![]))
            })
            .unwrap();
        terminal(&tasks, "v2-kept").await;
        for i in 0..MAX_TASKS - 1 {
            let id = format!("legacy-{i}");
            tasks
                .prepare_submit("test", id.clone(), binding(1), || async {
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
            .prepare_submit("test", "new-v2".into(), v2_binding(&tasks), || async {
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
            .prepare_submit("test", "unreadable".into(), original.clone(), || async {
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
                .prepare_submit("test", id.clone(), v2_binding(&tasks), || async {
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
                .prepare_submit("test", "unreadable".into(), original, || async {
                    panic!("unreadable old request must not replay")
                })
                .is_err()
        );
    }
}

#[cfg(test)]
#[path = "tasks/admission_tests.rs"]
mod admission_tests;
