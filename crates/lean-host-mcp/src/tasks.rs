//! Bounded, process-local request retention for MCP Tasks and `lean_request`.
//!
//! Cancellation signals the tool and waits for its cleanup; no expiry timer aborts
//! a worker operation. Finished results remain available for ten minutes. This is
//! reconnect support, not recovery after a server restart.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, DetailedTask, Task, TaskPayload, TaskStatus};
use rmcp::task_manager::current_timestamp;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::cancellation::with_cancellation;

const MAX_TASKS: usize = 128;
const RETENTION: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Default)]
pub(crate) struct RequestTasks(Arc<Mutex<HashMap<String, Entry>>>);

#[derive(Debug)]
struct Entry {
    task: DetailedTask,
    cancellation: CancellationToken,
    created: Instant,
    expires: Option<Instant>,
    submission: Option<RequestBinding>,
}

/// Immutable input identity, captured atomically with explicit submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RequestBinding {
    pub server_instance_id: String,
    pub tool: String,
    pub submission_sha256: String,
    /// Exact supplied project; source/canonical-root validation stays with callers.
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RetainedTask {
    #[serde(flatten)]
    task: DetailedTask,
    request: Option<RequestBinding>,
}

impl RequestTasks {
    pub(crate) fn status(&self) -> crate::status::RequestRegistryStatus {
        let mut entries = self.0.lock();
        prune(&mut entries, Instant::now());
        let active = entries
            .values()
            .filter(|entry| !entry.task.status().is_terminal())
            .count();
        crate::status::RequestRegistryStatus {
            active,
            retained_terminal: entries.len().saturating_sub(active),
            capacity: MAX_TASKS,
            terminal_retention_millis: u64::try_from(RETENTION.as_millis()).unwrap_or(u64::MAX),
        }
    }
    /// Register before spawning, so a returned handle is immediately readable.
    /// A full registry declines task mode; the caller can run synchronously.
    pub(crate) fn spawn<F, Fut>(&self, tool: &str, operation: F) -> Option<Task>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
    {
        self.spawn_inner(tool, None, operation).ok().flatten()
    }

    /// An explicitly identified submission never falls back to synchronous work.
    /// Reusing the same ID and inputs returns the original handle without running
    /// the operation again. Identity lasts only as long as the retained record.
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
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
        {
            return Err(McpError::invalid_params(
                "request_id must contain 1-128 ASCII letters, digits, underscores, hyphens, dots or colons",
                None,
            ));
        }
        self.spawn_inner(tool, Some((request_id, submission)), operation)?
            .ok_or_else(|| McpError::internal_error("Request registry full; no work was submitted", None))
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
            let mut entries = self.0.lock();
            prune(&mut entries, Instant::now());
            if let Some((request_id, submission)) = &identity
                && let Some(entry) = entries.get(request_id)
            {
                return if entry.submission.as_ref() == Some(submission) {
                    Ok(Some(entry.task.task.clone()))
                } else {
                    Err(McpError::invalid_params(
                        "request_id already belongs to a different submission",
                        None,
                    ))
                };
            }
            if entries.len() >= MAX_TASKS {
                return Ok(None);
            }
            let (task_id, submission) = identity.map_or_else(
                || (uuid::Uuid::new_v4().to_string(), None),
                |(task_id, submission)| (task_id, Some(submission)),
            );
            let now = current_timestamp();
            let task = Task::new(task_id, TaskStatus::Working, &now, &now)
                .with_status_message(format!("Accepted {tool}"))
                .with_poll_interval_ms(2_000);
            entries.insert(
                task.task_id.clone(),
                Entry {
                    task: DetailedTask::new(task.clone(), TaskPayload::Working),
                    cancellation: cancellation.clone(),
                    created: Instant::now(),
                    expires: None,
                    submission,
                },
            );
            task
        };
        let tasks = self.clone();
        let task_id = task.task_id.clone();
        tokio::spawn(async move {
            // The inner task catches panics; dropping its future also runs the
            // cancellation guard. Ordinary cancellation never aborts this task.
            let result = tokio::spawn(async move { with_cancellation(&cancellation, operation()).await })
                .await
                .unwrap_or_else(|error| Err(McpError::internal_error(format!("task failed: {error}"), None)));
            tasks.finish(&task_id, result);
            let entries = Arc::downgrade(&tasks.0);
            drop(tasks);
            tokio::time::sleep(RETENTION).await;
            if let Some(entries) = entries.upgrade() {
                prune(&mut entries.lock(), Instant::now());
            }
        });
        Ok(Some(task))
    }

    pub(crate) fn get(&self, task_id: &str) -> Result<DetailedTask, McpError> {
        let mut entries = self.0.lock();
        prune(&mut entries, Instant::now());
        entries
            .get(task_id)
            .map(|entry| entry.task.clone())
            .ok_or_else(unknown_task)
    }

    pub(crate) fn get_retained(&self, task_id: &str) -> Result<RetainedTask, McpError> {
        let mut entries = self.0.lock();
        prune(&mut entries, Instant::now());
        entries
            .get(task_id)
            .map(|entry| RetainedTask {
                task: entry.task.clone(),
                request: entry.submission.clone(),
            })
            .ok_or_else(unknown_task)
    }

    pub(crate) fn cancel(&self, task_id: &str) -> Result<(), McpError> {
        let mut entries = self.0.lock();
        prune(&mut entries, Instant::now());
        let entry = entries.get_mut(task_id).ok_or_else(unknown_task)?;
        if !entry.task.status().is_terminal() {
            entry.cancellation.cancel();
            entry.task.task.status_message = Some("Cancellation requested; waiting for work to stop".to_owned());
            entry.task.task.last_updated_at = current_timestamp();
        }
        drop(entries);
        Ok(())
    }

    fn finish(&self, task_id: &str, result: Result<CallToolResult, McpError>) {
        let mut entries = self.0.lock();
        let Some(entry) = entries.get_mut(task_id) else { return };
        let payload = if entry.cancellation.is_cancelled() {
            TaskPayload::Cancelled
        } else {
            match result {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(serde_json::Value::Object(result)) => TaskPayload::Completed { result },
                    _ => TaskPayload::Failed {
                        error: serde_json::Map::from_iter([
                            ("code".to_owned(), serde_json::json!(-32603)),
                            (
                                "message".to_owned(),
                                serde_json::json!("could not serialize tool result"),
                            ),
                        ]),
                    },
                },
                Err(error) => TaskPayload::Failed {
                    error: serde_json::Map::from_iter([
                        ("code".to_owned(), serde_json::json!(error.code)),
                        ("message".to_owned(), serde_json::json!(error.message)),
                        ("data".to_owned(), serde_json::json!(error.data)),
                    ]),
                },
            }
        };
        entry.task.task.last_updated_at = current_timestamp();
        entry.task.task.status_message = None;
        entry.task.task.ttl_ms =
            Some(u64::try_from(entry.created.elapsed().saturating_add(RETENTION).as_millis()).unwrap_or(u64::MAX));
        entry.task = DetailedTask::new(entry.task.task.clone(), payload);
        entry.expires = Instant::now().checked_add(RETENTION);
        drop(entries);
    }
}

fn prune(entries: &mut HashMap<String, Entry>, now: Instant) {
    entries.retain(|_, entry| entry.expires.is_none_or(|expiry| expiry > now));
}

fn unknown_task() -> McpError {
    McpError::invalid_params("Unknown or expired task", None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn binding(byte: u8) -> RequestBinding {
        RequestBinding {
            server_instance_id: "test-instance".to_owned(),
            tool: "test".to_owned(),
            submission_sha256: format!("{byte:02x}").repeat(32),
            project: Some("/test/project".to_owned()),
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
        assert_eq!(tasks.0.lock().len(), MAX_TASKS);
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
        assert_eq!(terminal(&tasks, &task.task_id).await.status(), TaskStatus::Cancelled);
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
        prune(&mut clone.0.lock(), Instant::now() + RETENTION + Duration::from_secs(1));
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
        assert_eq!(tasks.0.lock().len(), MAX_TASKS);
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
        prune(&mut tasks.0.lock(), Instant::now() + RETENTION + Duration::from_secs(1));
        assert_eq!(tasks.get(&task.task_id).unwrap().status(), TaskStatus::Working);
        finish_tx.send(()).unwrap();
        let TaskPayload::Failed { error } = terminal(&tasks, &task.task_id).await.payload else {
            panic!("expected protocol error")
        };
        assert_eq!(error.get("message"), Some(&serde_json::json!("invalid project")));
        assert_eq!(error.get("data"), Some(&serde_json::json!({"detail": 1})));
    }
}
