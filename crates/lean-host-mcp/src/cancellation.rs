//! Request cancellation shared by async callers and the synchronous worker owner.

use std::future::Future;

use lean_rs_worker_parent::LeanWorkerCancellationToken;
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static CURRENT: CallCancellation;
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CallCancellation {
    worker: LeanWorkerCancellationToken,
    signal: CancellationToken,
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
    let cancellation = CallCancellation::default();
    let _on_drop = cancellation.cancel_on_drop();
    if request.is_cancelled() {
        cancellation.cancel();
    }
    CURRENT
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
        .await
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
}
