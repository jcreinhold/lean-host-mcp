//! Bounded, process-local request retention for the negotiated MCP Tasks extension.
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
}

impl RequestTasks {
    /// Register before spawning, so a returned handle is immediately readable.
    /// A full registry declines task mode; the caller can run synchronously.
    pub(crate) fn spawn<F, Fut>(&self, tool: &str, operation: F) -> Option<Task>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CallToolResult, McpError>> + Send + 'static,
    {
        let cancellation = CancellationToken::new();
        let task = {
            let mut entries = self.0.lock();
            prune(&mut entries, Instant::now());
            if entries.len() >= MAX_TASKS {
                return None;
            }
            let now = current_timestamp();
            let task = Task::new(uuid::Uuid::new_v4().to_string(), TaskStatus::Working, &now, &now)
                .with_status_message(format!("Accepted {tool}"))
                .with_poll_interval_ms(2_000);
            entries.insert(
                task.task_id.clone(),
                Entry {
                    task: DetailedTask::new(task.clone(), TaskPayload::Working),
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
        Some(task)
    }

    pub(crate) fn get(&self, task_id: &str) -> Result<DetailedTask, McpError> {
        let mut entries = self.0.lock();
        prune(&mut entries, Instant::now());
        entries
            .get(task_id)
            .map(|entry| entry.task.clone())
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
