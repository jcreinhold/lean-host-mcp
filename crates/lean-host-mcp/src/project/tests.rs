//! The project actor's mailbox admission, queue deadlines and cancellation.

use super::*;

pub(super) async fn poll_pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn mailbox_admission_is_fifo_and_cancellation_removes_only_its_waiter() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(0).await.unwrap();
    let first_cancel = CallCancellation::default();
    let middle_cancel = CallCancellation::default();
    let last_cancel = CallCancellation::default();
    let first = reserve_mailbox(&tx, &first_cancel, Duration::from_secs(5));
    let middle = reserve_mailbox(&tx, &middle_cancel, Duration::from_secs(5));
    let last = reserve_mailbox(&tx, &last_cancel, Duration::from_secs(5));
    tokio::pin!(first, middle, last);
    poll_pending(first.as_mut()).await;
    poll_pending(middle.as_mut()).await;
    poll_pending(last.as_mut()).await;
    middle_cancel.cancel();
    assert_eq!(middle.await.err(), Some("request_cancelled"));
    assert_eq!(rx.recv().await, Some(0));
    first.await.unwrap().send(1);
    poll_pending(last.as_mut()).await;
    assert_eq!(rx.recv().await, Some(1));
    last.await.unwrap().send(3);
    assert_eq!(rx.recv().await, Some(3));
}

#[tokio::test]
async fn mailbox_admission_timeout_and_closed_channel_never_enqueue() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(0).await.unwrap();
    let cancellation = CallCancellation::default();
    assert_eq!(
        reserve_mailbox(&tx, &cancellation, Duration::from_millis(1))
            .await
            .err(),
        Some("mailbox_admission_timeout")
    );
    assert_eq!(rx.recv().await, Some(0));
    assert!(rx.try_recv().is_err());
    drop(rx);
    assert_eq!(
        reserve_mailbox(&tx, &cancellation, Duration::from_secs(1)).await.err(),
        Some("mailbox_closed")
    );
}

fn test_admission_deadline(limit: Duration) -> Instant {
    Instant::now().checked_add(limit).unwrap()
}

fn queued_test_project(limit: Duration) -> (LeanProject, mpsc::Receiver<ProjectMessage>, Instant) {
    let (tx, rx) = mpsc::channel(2);
    let project = LeanProject {
        canonical_root: PathBuf::from("/test"),
        toolchain: "test".into(),
        manifest_hash: String::new(),
        session_id: String::new(),
        open_warnings: Vec::new(),
        actor_tx: Mutex::new(Some(tx)),
        active_jobs: Arc::new(AtomicUsize::new(0)),
        pool_active_jobs: Arc::new(AtomicUsize::new(0)),
        healthy: Arc::new(AtomicBool::new(true)),
        runtime: Arc::new(Mutex::new(RuntimeSnapshot {
            protected_artifact_domains: BTreeSet::new(),
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
            import_residue_bytes: None,
            import_residue_limit_bytes: None,
        })),
        module_queries: ModuleQueryCache::with_capacity(NonZeroUsize::MIN),
        actor_scope: std::sync::Weak::new(),
        process_tracker: LeanWorkerProcessTracker::new(),
    };
    (project, rx, test_admission_deadline(limit))
}

#[tokio::test]
async fn queue_timeout_returns_before_actor_drains_and_prevents_dispatch() {
    let (project, mut rx, admission_deadline) = queued_test_project(Duration::from_millis(5));
    let (reply, receive) = oneshot::channel();
    let message = ProjectMessage::ModuleQuery {
        meta: project.job_meta(vec![], RetryPolicy::Never, admission_deadline),
        source: String::new(),
        query: LeanWorkerModuleQuery::Diagnostics,
        options: LeanWorkerElabOptions::default(),
        reply,
    };
    let result = tokio::time::timeout(Duration::from_secs(1), project.enqueue(message, receive))
        .await
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("queue_wait_timeout"));
    let message = rx.recv().await.unwrap();
    assert!(!message.meta().queue.start());
    drop(message);
    assert_eq!(project.active_jobs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn queue_deadline_includes_both_admission_and_actor_wait() {
    let (project, mut rx, admission_deadline) = queued_test_project(Duration::from_millis(500));
    let tx = project.actor_tx.lock().as_ref().unwrap().clone();
    // Occupy every mailbox slot without dispatching a worker request.
    let permits = tx.reserve_many(2).await.unwrap();
    let (reply, receive) = oneshot::channel();
    let message = ProjectMessage::ModuleQuery {
        meta: project.job_meta(vec![], RetryPolicy::Never, admission_deadline),
        source: String::new(),
        query: LeanWorkerModuleQuery::Diagnostics,
        options: LeanWorkerElabOptions::default(),
        reply,
    };
    let mut call = Box::pin(project.enqueue(message, receive));
    tokio::select! {
        biased;
        _ = &mut call => panic!("must wait for mailbox admission"),
        () = tokio::time::sleep(Duration::from_millis(300)) => {}
    }
    drop(permits);
    // The actor never drains the request. The original deadline must win;
    // granting a fresh 500ms after admission would fail this bound.
    let result = tokio::time::timeout(Duration::from_millis(350), &mut call)
        .await
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("queue_wait_timeout"));
    let message = rx.recv().await.unwrap();
    assert!(!message.meta().queue.start());
}

#[tokio::test]
async fn dropping_queued_caller_cancels_message_before_dispatch() {
    let (project, mut rx, admission_deadline) = queued_test_project(Duration::from_secs(1));
    let (reply, receive) = oneshot::channel();
    let message = ProjectMessage::ModuleQuery {
        meta: project.job_meta(vec![], RetryPolicy::Never, admission_deadline),
        source: String::new(),
        query: LeanWorkerModuleQuery::Diagnostics,
        options: LeanWorkerElabOptions::default(),
        reply,
    };
    let mut call = Box::pin(project.enqueue(message, receive));
    tokio::select! { biased; _ = &mut call => panic!("must wait"), () = tokio::task::yield_now() => {} }
    drop(call);
    let message = rx.recv().await.unwrap();
    // The actor checks cancellation before the atomic start decision.
    assert!(message.meta().cancellation.is_cancelled());
    drop(message);
    assert_eq!(project.active_jobs.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn cancellation_of_running_message_waits_for_actor_cleanup() {
    let (project, mut rx, admission_deadline) = queued_test_project(Duration::from_secs(1));
    let (reply, receive) = oneshot::channel();
    let message = ProjectMessage::ModuleQuery {
        meta: project.job_meta(vec![], RetryPolicy::Never, admission_deadline),
        source: String::new(),
        query: LeanWorkerModuleQuery::Diagnostics,
        options: LeanWorkerElabOptions::default(),
        reply,
    };
    let mut call = Box::pin(project.enqueue(message, receive));
    tokio::select! { biased; _ = &mut call => panic!("must wait"), () = tokio::task::yield_now() => {} }
    let message = rx.recv().await.unwrap();
    assert!(message.meta().queue.start());
    message.meta().cancellation.cancel();
    assert!(tokio::time::timeout(Duration::from_millis(5), &mut call).await.is_err());
    if let ProjectMessage::ModuleQuery { reply, .. } = message {
        let _ = reply.send(Err(ServerError::Internal("stopped, outcome unknown".into())));
    }
    assert!(call.await.unwrap_err().to_string().contains("outcome unknown"));
}

#[test]
fn expired_queue_entry_never_starts() {
    let queue = QueueDeadline::new(Instant::now());
    assert!(!queue.start());
    assert!(queue.expire());
    assert!(!queue.start());
}

#[test]
fn cancelled_waiting_queue_entry_never_starts() {
    let queue = QueueDeadline::new(test_admission_deadline(Duration::from_secs(30)));
    assert!(queue.expire());
    assert!(!queue.start());
}

#[test]
fn started_job_is_not_expired_by_queue_deadline() {
    let queue = QueueDeadline::new(test_admission_deadline(Duration::from_secs(30)));
    assert!(queue.start());
    assert!(!queue.expire());
    assert!(!queue.start());
}

#[test]
fn queue_expiry_race_has_only_one_winner() {
    for _ in 0..100 {
        let queue = Arc::new(QueueDeadline::new(test_admission_deadline(Duration::from_secs(30))));
        let other = Arc::clone(&queue);
        let start = std::thread::spawn(move || other.start());
        let expired = queue.expire();
        let started = start.join().unwrap();
        assert_ne!(expired, started);
    }
}

#[cfg(unix)]
#[test]
fn real_child_timeout_binds_stop_to_the_interrupted_generation_only() {
    use lean_rs_worker_parent::{LeanWorker, LeanWorkerConfig};
    use lean_rs_worker_protocol::protocol::{MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, write_frame};
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let mut handshake = Vec::new();
    write_frame(
        &mut handshake,
        Message::Handshake {
            worker_version: "stop-receipt-test".into(),
            protocol_version: PROTOCOL_VERSION,
        },
        MAX_FRAME_BYTES,
    )
    .unwrap();
    std::fs::write(root.path().join("handshake"), handshake).unwrap();
    let executable = root.path().join("fake-worker");
    std::fs::write(&executable, "#!/bin/sh\n/bin/cat handshake\nexec /bin/sleep 60\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tracker = LeanWorkerProcessTracker::new();
    let mut worker = LeanWorker::spawn(
        &LeanWorkerConfig::new(executable)
            .current_dir(root.path())
            .process_tracker(tracker.clone())
            .startup_timeout(Duration::from_secs(5))
            .shutdown_timeout(Duration::from_millis(20))
            .request_timeout(Duration::from_millis(30)),
    )
    .unwrap();
    let before = worker.lifecycle_snapshot();
    let checkpoint = worker.request_checkpoint();
    let error = worker.health().unwrap_err();
    assert!(matches!(error, LeanWorkerError::Timeout { .. }));
    let operation_id = uuid::Uuid::new_v4();
    let upstream = worker.confirmed_request_stop_since(&checkpoint).unwrap();
    let receipt = confirmed_stop_receipt("session", operation_id, "main", 1, upstream, "timeout");
    assert_eq!(receipt.operation_id, operation_id.to_string());
    assert_eq!(receipt.worker_session_id, "session");
    assert_eq!(receipt.worker_generation, 1 + before.worker_generation);
    assert_eq!(receipt.operation, "health");
    assert_eq!(
        tracker.active_processes(),
        1,
        "only the healthy replacement is still owned"
    );
    assert_eq!(receipt.worker_scope, "main");
    assert_eq!(receipt.worker_request_id, Some(upstream.request_id()));
    assert_eq!(receipt.worker_pid, Some(upstream.process_id()));
    assert_eq!(receipt.exit_success, Some(false));
    assert_eq!(receipt.exit_status.as_deref(), Some(upstream.exit().status.as_str()));
    assert!(
        worker
            .confirmed_request_stop_since(&worker.request_checkpoint())
            .is_none(),
        "a historical stop cannot certify a fresh checkpoint"
    );
    worker.shutdown().unwrap();
    assert_eq!(tracker.active_processes(), 0);
}

#[test]
fn cancelled_queued_messages_keep_their_cancellation_and_release_activity_on_drop() {
    let cancellation = CallCancellation::default();
    let active = Arc::new(AtomicUsize::new(1));
    let pool_active = Arc::new(AtomicUsize::new(1));
    let (reply, _receive) = oneshot::channel();
    let message = ProjectMessage::ModuleQuery {
        meta: JobMeta {
            file_label: None,
            queue: Arc::new(QueueDeadline::new(test_admission_deadline(Duration::from_secs(30)))),
            imports: vec!["Init".to_owned()],
            import_fingerprint: "Init".to_owned(),
            _created_at: Instant::now(),
            queued_at: Instant::now(),
            correlation_id: uuid::Uuid::new_v4(),
            retry_policy: RetryPolicy::Never,
            _active_job: ActiveJobGuard {
                active_jobs: Arc::clone(&active),
                pool_active_jobs: Arc::clone(&pool_active),
            },
            cancellation: cancellation.clone(),
            granted_at: None,
        },
        source: String::new(),
        query: LeanWorkerModuleQuery::Diagnostics,
        options: LeanWorkerElabOptions::default(),
        reply,
    };
    cancellation.cancel();
    assert!(message.meta().cancellation.is_cancelled());
    drop(message);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert_eq!(pool_active.load(Ordering::Acquire), 0);
}
