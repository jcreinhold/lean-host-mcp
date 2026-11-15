//! Request cancellation shared by async callers and the synchronous worker owner.

use std::future::Future;
use std::sync::Arc;

use lean_rs_worker_parent::LeanWorkerCancellationToken;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::envelope::{TargetCheckNotStarted, WorkerStopReceipt};

tokio::task_local! {
    static CURRENT: CallCancellation;
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CallCancellation {
    worker: LeanWorkerCancellationToken,
    signal: CancellationToken,
    execution: Arc<Mutex<CallExecutionEvidence>>,
}

/// Aggregate operation evidence, independent of semantic results and carriers.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallExecutionEvidence {
    pub(crate) pending: usize,
    pub(crate) returned: usize,
    pub(crate) stopped: usize,
    pub(crate) not_started: usize,
    pub(crate) uncertain: bool,
    pub(crate) worker_stop_receipt: Option<WorkerStopReceipt>,
    pub(crate) target_check: Option<TargetCheckNotStarted>,
}

/// Each enqueued operation owns its own terminal evidence. A later successful
/// fallback cannot settle an earlier operation whose cleanup remains unknown.
pub(crate) struct CallOperation(Option<Arc<Mutex<CallExecutionEvidence>>>);

impl CallOperation {
    pub(crate) fn not_started(self) {
        self.resolve(|evidence| evidence.not_started = evidence.not_started.saturating_add(1));
    }

    pub(crate) fn returned(self) {
        self.resolve(|evidence| evidence.returned = evidence.returned.saturating_add(1));
    }

    pub(crate) fn stopped(self, receipt: WorkerStopReceipt) {
        self.resolve(|evidence| {
            evidence.stopped = evidence.stopped.saturating_add(1);
            evidence.worker_stop_receipt = Some(receipt);
        });
    }

    pub(crate) fn unknown(self) {
        self.resolve(|evidence| evidence.uncertain = true);
    }

    fn resolve(mut self, update: impl FnOnce(&mut CallExecutionEvidence)) {
        if let Some(execution) = self.0.take() {
            let mut evidence = execution.lock();
            evidence.pending = evidence.pending.saturating_sub(1);
            update(&mut evidence);
            drop(evidence);
        }
    }
}

impl Drop for CallOperation {
    fn drop(&mut self) {
        if let Some(execution) = self.0.take() {
            let mut evidence = execution.lock();
            evidence.pending = evidence.pending.saturating_sub(1);
            evidence.uncertain = true;
        }
    }
}

impl CallCancellation {
    pub(crate) fn current() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_default()
    }

    pub(crate) fn worker(&self) -> &LeanWorkerCancellationToken {
        &self.worker
    }

    pub(crate) fn cancel(&self) {
        self.worker.cancel();
        self.signal.cancel();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.worker.is_cancelled()
    }

    pub(crate) async fn cancelled(&self) {
        self.signal.cancelled().await;
    }

    /// Retain uncertainty even when a tool degrades a failed worker call into
    /// another successful semantic result. The outer lifecycle must not lose it.
    pub(crate) fn mark_uncertain(&self) {
        self.execution.lock().uncertain = true;
    }

    /// Preparation can prove a named target check never ran without proving
    /// that every subprocess started during preparation has stopped.
    pub(crate) fn preparation_failed(&self, target_check: TargetCheckNotStarted) {
        self.mark_uncertain();
        let mut evidence = self.execution.lock();
        evidence.target_check = Some(target_check);
    }

    pub(crate) fn begin_operation(&self) -> CallOperation {
        let mut evidence = self.execution.lock();
        evidence.pending = evidence.pending.saturating_add(1);
        drop(evidence);
        CallOperation(Some(Arc::clone(&self.execution)))
    }

    pub(crate) fn cancel_on_drop(&self) -> CancelOnDrop {
        CancelOnDrop(Some(self.clone()))
    }
}

/// A dropped reply future still cancels its queued or running worker request.
pub(crate) struct CancelOnDrop(Option<CallCancellation>);

impl CancelOnDrop {
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
    }
}

/// Signal cancellation without dropping the operation that owns terminal cleanup.
pub(crate) async fn with_cancellation<T>(request: &CancellationToken, operation: impl Future<Output = T>) -> T {
    with_cancellation_evidence(request, operation).await.0
}

pub(crate) async fn with_cancellation_evidence<T>(
    request: &CancellationToken,
    operation: impl Future<Output = T>,
) -> (T, CallExecutionEvidence) {
    let cancellation = CallCancellation::default();
    let _on_drop = cancellation.cancel_on_drop();
    if request.is_cancelled() {
        cancellation.cancel();
    }
    let result = CURRENT
        .scope(cancellation.clone(), async {
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                () = request.cancelled() => {
                    cancellation.cancel();
                    operation.await
                }
            }
        })
        .await;
    let evidence = cancellation.execution.lock().clone();
    (result, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_waits_for_terminal_cleanup_and_reaches_worker() {
        let request = CancellationToken::new();
        let operation = async {
            let current = CallCancellation::current();
            request.cancel();
            current.cancelled().await;
            assert!(current.worker().is_cancelled());
            // Completion must come from the operation, not a dropped future.
            42
        };
        assert_eq!(with_cancellation(&request, operation).await, 42);
    }

    #[tokio::test]
    async fn already_cancelled_request_never_appears_live() {
        let request = CancellationToken::new();
        request.cancel();
        with_cancellation(&request, async {
            assert!(CallCancellation::current().is_cancelled());
        })
        .await;
    }

    #[tokio::test]
    async fn dropping_call_cancels_owned_work_without_cancelling_other_calls() -> Result<(), Box<dyn std::error::Error>>
    {
        let request = CancellationToken::new();
        let (send, receive) = tokio::sync::oneshot::channel();
        let call = tokio::spawn(async move {
            with_cancellation(&request, async {
                drop(send.send(CallCancellation::current()));
                std::future::pending::<()>().await;
            })
            .await;
        });
        let owned = receive.await?;
        let unrelated = CallCancellation::current();
        call.abort();
        drop(call.await);
        assert!(owned.is_cancelled());
        assert!(!unrelated.is_cancelled());
        Ok(())
    }

    #[test]
    fn completed_reply_does_not_cancel_later_work_in_the_same_call() {
        let cancellation = CallCancellation::default();
        let mut guard = cancellation.cancel_on_drop();
        guard.disarm();
        drop(guard);
        assert!(!cancellation.is_cancelled());
    }

    #[test]
    fn abandoned_operation_stays_uncertain_after_a_later_return() {
        let cancellation = CallCancellation::default();
        drop(cancellation.begin_operation());
        cancellation.begin_operation().returned();
        let evidence = cancellation.execution.lock();
        assert!(evidence.uncertain);
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.returned, 1);
    }

    #[test]
    fn operation_resolutions_are_independent() {
        let cancellation = CallCancellation::default();
        let pending = cancellation.begin_operation();
        cancellation.begin_operation().not_started();
        cancellation.begin_operation().returned();
        {
            let evidence = cancellation.execution.lock();
            assert_eq!(evidence.pending, 1);
            assert_eq!(evidence.not_started, 1);
            assert_eq!(evidence.returned, 1);
            assert!(!evidence.uncertain);
            drop(evidence);
        }
        pending.unknown();
        assert!(cancellation.execution.lock().uncertain);
    }

    #[tokio::test]
    async fn scoped_execution_evidence_keeps_the_stopped_worker_identity() {
        let request = CancellationToken::new();
        let receipt = WorkerStopReceipt {
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
        };
        let ((), evidence) = with_cancellation_evidence(&request, async {
            CallCancellation::current().begin_operation().stopped(receipt.clone());
        })
        .await;
        assert_eq!(evidence.worker_stop_receipt, Some(receipt));
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.stopped, 1);
        assert!(!evidence.uncertain);
    }
}
