//! Admission races and at-most-once execution of server-reserved tickets.
//!
//! Before v3, two fresh submissions at 125 terminal + 3 active records sharing
//! one observed epoch yielded one acceptance and one closed-epoch rejection.
//! Expiring an unrelated result between status and submission also rejected a
//! never-dispatched fresh ID as unknown. Both were reproduced in the frozen
//! pre-S2 checkout; these tests protect the replacement ticket contract.

#![allow(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

fn binding() -> RequestBinding {
    RequestBinding {
        server_instance_id: "admission-test-server".into(),
        tool: "test".into(),
        submission_sha256: "01".repeat(32),
        project: Some("/test/admission".into()),
        request_epoch: None,
    }
}

fn unpack_prepared(prepared: RetainedTask) -> (String, RequestBinding) {
    let id = prepared.fields["taskId"].as_str().expect("server ticket ID").to_owned();
    let binding = prepared.request.expect("reserved immutable input");
    let epoch = binding.request_epoch.as_deref().expect("server admission identity");
    assert!(id.starts_with(&format!("{epoch}:")));
    assert!(!prepared.result_available);
    (id, binding)
}

fn prepare_submission(tasks: &RequestTasks) -> (String, RequestBinding) {
    unpack_prepared(tasks.prepare(binding()).expect("reserve request ticket"))
}

async fn terminal(tasks: &RequestTasks, id: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if tasks.get(id).expect("retained task").status().is_terminal() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operation became terminal");
}

async fn mixed_registry(
    terminal_count: usize,
) -> (RequestTasks, Vec<String>, Vec<String>, Arc<tokio::sync::Semaphore>) {
    let tasks = RequestTasks::default();
    let mut terminal_ids = Vec::new();
    for _ in 0..terminal_count {
        let (id, binding) = prepare_submission(&tasks);
        tasks
            .submit("test", id.clone(), binding, || async {
                Ok(CallToolResult::success(vec![]))
            })
            .expect("complete terminal fixture");
        terminal(&tasks, &id).await;
        terminal_ids.push(id);
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (started, mut observed) = tokio::sync::mpsc::channel(3);
    let mut active_ids = Vec::new();
    for _ in 0..3 {
        let (id, binding) = prepare_submission(&tasks);
        let gate = Arc::clone(&gate);
        let started = started.clone();
        tasks
            .submit("test", id.clone(), binding, move || async move {
                started.send(()).await.expect("observe active request");
                let _permit = gate.acquire_owned().await.expect("release active request");
                Ok(CallToolResult::success(vec![]))
            })
            .expect("admit active request");
        active_ids.push(id);
    }
    for _ in 0..3 {
        tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .expect("active request started")
            .expect("active request observation");
    }
    let status = tasks.status();
    assert_eq!(status.active, 3);
    assert_eq!(status.retained_terminal, terminal_count);
    (tasks, terminal_ids, active_ids, gate)
}

async fn finish_active(tasks: &RequestTasks, ids: Vec<String>, gate: &tokio::sync::Semaphore) {
    gate.add_permits(ids.len());
    for id in ids {
        terminal(tasks, &id).await;
    }
}

fn assert_no_start(error: &McpError, reason: &str) {
    let data = error.data.as_ref().expect("structured rejection");
    assert_eq!(data["reason"], reason);
    assert_eq!(data["execution"]["disposition"], "not_started");
}

#[tokio::test]
async fn mixed_pressure_backpressures_concurrent_preparation_without_eviction_or_epoch_rotation() {
    let (tasks, terminal_ids, active_ids, gate) = mixed_registry(MAX_TASKS - 3).await;
    let observed_epoch = tasks.status().request_epoch;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let tasks = tasks.clone();
        let barrier = Arc::clone(&barrier);
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            tasks.prepare(binding())
        }));
    }
    barrier.wait().await;
    for attempt in attempts {
        let error = attempt
            .await
            .expect("preparing client")
            .expect_err("unacknowledged results retain capacity");
        assert_no_start(&error, "request_registry_full");
    }
    let status = tasks.status();
    assert_eq!(status.request_epoch, observed_epoch);
    assert_eq!(status.active, 3);
    assert_eq!(status.retained_terminal, 125);
    assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
    for id in terminal_ids {
        assert!(
            tasks
                .get_retained(&id)
                .expect("unconsumed result retained")
                .result_available
        );
    }
    finish_active(&tasks, active_ids, &gate).await;
}

#[tokio::test]
async fn concurrent_preparation_reserves_the_final_slot_once_without_dispatch() {
    let (tasks, terminal_ids, active_ids, gate) = mixed_registry(MAX_TASKS - 4).await;
    let observed_epoch = tasks.status().request_epoch;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let tasks = tasks.clone();
        let barrier = Arc::clone(&barrier);
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            tasks.prepare(binding())
        }));
    }
    barrier.wait().await;
    let mut accepted = Vec::new();
    let mut rejected = 0;
    for attempt in attempts {
        match attempt.await.expect("preparing client") {
            Ok(ticket) => accepted.push(unpack_prepared(ticket)),
            Err(error) => {
                assert_no_start(&error, "request_registry_full");
                rejected += 1;
            }
        }
    }
    assert_eq!(accepted.len(), 1);
    assert_eq!(rejected, 1);
    assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
    assert_eq!(tasks.status().request_epoch, observed_epoch);
    let (id, binding) = accepted.pop().expect("one prepared ticket");
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let count = Arc::clone(&calls);
    tasks
        .submit("test", id.clone(), binding, move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(CallToolResult::success(vec![]))
        })
        .expect("reserved slot survives pressure until submission");
    terminal(&tasks, &id).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for id in terminal_ids {
        assert!(
            tasks
                .get_retained(&id)
                .expect("original result retained")
                .result_available
        );
    }
    finish_active(&tasks, active_ids, &gate).await;
}

#[tokio::test]
async fn unrelated_expiry_between_preparation_and_submission_preserves_ticket_identity() {
    let tasks = RequestTasks::default();
    let (old_id, old_binding) = prepare_submission(&tasks);
    tasks
        .submit("test", old_id.clone(), old_binding.clone(), || async {
            Ok(CallToolResult::success(vec![]))
        })
        .expect("complete expiring fixture");
    terminal(&tasks, &old_id).await;
    let (id, binding) = prepare_submission(&tasks);
    let observed_epoch = tasks.status().request_epoch;
    tasks
        .0
        .lock()
        .entries
        .get_mut(&old_id)
        .expect("terminal fixture")
        .expires = Some(Instant::now());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    tasks
        .submit("test", id.clone(), binding, move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(CallToolResult::success(vec![]))
        })
        .expect("unrelated expiry cannot invalidate a live reservation");
    terminal(&tasks, &id).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(tasks.status().request_epoch, observed_epoch);
    let error = tasks
        .submit("test", old_id, old_binding, || async {
            panic!("expired ticket must not be admitted again")
        })
        .expect_err("forgotten ticket remains unknown");
    assert_eq!(
        error.data.expect("unknown outcome")["execution"]["disposition"],
        "unknown"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_prepare_response_can_expire_without_dispatch_and_never_revive() {
    let tasks = RequestTasks::default();
    // Only the test oracle knows this first response; the client lost it before
    // saving any submit intent, so preparing another ticket is safe.
    let (lost_id, lost_binding) = prepare_submission(&tasks);
    let (id, binding) = prepare_submission(&tasks);
    assert_ne!(id, lost_id);
    let calls = Arc::new(AtomicUsize::new(0));
    tasks
        .0
        .lock()
        .entries
        .get_mut(&lost_id)
        .expect("orphan reservation")
        .expires = Some(Instant::now());
    let orphan = tasks
        .get_retained(&lost_id)
        .expect("expired preparation retains a no-start receipt");
    assert_eq!(orphan.execution.disposition, Disposition::NotStarted);
    assert!(!orphan.result_available);
    let retry = tasks
        .submit("test", lost_id, lost_binding, || async {
            panic!("expired orphan ticket must never dispatch")
        })
        .expect("recover terminal no-start receipt");
    assert_eq!(retry.status, TaskStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let count = Arc::clone(&calls);
    tasks
        .submit("test", id.clone(), binding, move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(CallToolResult::success(vec![]))
        })
        .expect("submit replacement reservation with saved intent");
    terminal(&tasks, &id).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_preparation_is_reclaimed_under_pressure_without_reusing_its_ticket() {
    let tasks = RequestTasks::default();
    let (expired_id, expired_binding) = prepare_submission(&tasks);
    let observed_epoch = tasks.status().request_epoch;
    tasks
        .0
        .lock()
        .entries
        .get_mut(&expired_id)
        .expect("prepared fixture")
        .expires = Some(Instant::now());
    assert_eq!(
        tasks
            .get_retained(&expired_id)
            .expect("expiry receipt")
            .execution
            .disposition,
        Disposition::NotStarted
    );
    let mut issued = std::collections::HashSet::from([expired_id.clone()]);
    for _ in 0..MAX_TASKS {
        let (id, _) = prepare_submission(&tasks);
        assert!(issued.insert(id), "server ticket sequence must never be reused");
    }
    assert_eq!(tasks.0.lock().entries.len(), MAX_TASKS);
    assert_eq!(tasks.status().request_epoch, observed_epoch);
    let error = tasks
        .submit("test", expired_id, expired_binding, || async {
            panic!("reclaimed preparation must not become new work")
        })
        .expect_err("reclaimed ticket is absent, never admissible");
    assert_eq!(
        error.data.expect("unknown outcome")["execution"]["disposition"],
        "unknown"
    );
    let full = tasks
        .prepare(binding())
        .expect_err("live preparations also consume bounded capacity");
    assert_no_start(&full, "request_registry_full");
}

#[tokio::test]
async fn restart_after_preparation_retains_not_started_and_does_not_submit_automatically() {
    let dir = tempfile::tempdir().expect("request journal");
    let tasks = RequestTasks::open(dir.path()).expect("open journal");
    let (id, original) = prepare_submission(&tasks);
    drop(tasks);
    let recovered = RequestTasks::open(dir.path()).expect("restart prepared journal");
    let receipt = recovered.get_retained(&id).expect("durable preparation recovered");
    assert_eq!(receipt.execution.disposition, Disposition::NotStarted);
    assert!(!receipt.result_available);
    assert_eq!(receipt.request, Some(original.clone()));
    let mut retry = original;
    retry.server_instance_id = "restarted-server".into();
    let result = recovered
        .submit("test", id, retry, || async {
            panic!("recovered unsubmitted ticket must never replay")
        })
        .expect("recover no-start result of original reservation");
    assert_eq!(result.status, TaskStatus::Failed);
}

#[tokio::test]
async fn concurrent_recovery_of_lost_acceptance_dispatches_the_same_identity_once() {
    let tasks = RequestTasks::default();
    let (id, original) = prepare_submission(&tasks);
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let tasks = tasks.clone();
        let id = id.clone();
        let original = original.clone();
        let barrier = Arc::clone(&barrier);
        let calls = Arc::clone(&calls);
        let release = Arc::clone(&release);
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            tasks.submit("test", id.clone(), original, move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let _permit = release.acquire_owned().await.expect("finish admitted operation");
                Ok(CallToolResult::success(vec![]))
            })
        }));
    }
    barrier.wait().await;
    for attempt in attempts {
        // Discard each acceptance as if neither network response arrived.
        let accepted = attempt
            .await
            .expect("recovering client")
            .expect("same identity is retained");
        assert_eq!(accepted.task_id, id);
    }
    let recovered = tasks
        .submit("test", id.clone(), original.clone(), || async {
            panic!("a lost acceptance must recover the original operation")
        })
        .expect("recover accepted identity");
    assert_eq!(recovered.task_id, id);
    release.add_permits(1);
    terminal(&tasks, &id).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let mut conflicting = original;
    conflicting.submission_sha256 = "02".repeat(32);
    let error = tasks
        .submit("test", id.clone(), conflicting, || async {
            panic!("conflicting identity must never dispatch")
        })
        .expect_err("changed arguments cannot recover the same identity");
    assert_eq!(
        error.data.expect("conflict evidence")["reason"],
        "request_identity_conflict"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_acknowledgement_response_and_restart_preserve_no_replay_tombstone() {
    let dir = tempfile::tempdir().expect("request journal");
    let tasks = RequestTasks::open(dir.path()).expect("open journal");
    let (id, original) = prepare_submission(&tasks);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&calls);
    tasks
        .submit("test", id.clone(), original.clone(), move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(CallToolResult::success(vec![]))
        })
        .expect("accept operation");
    terminal(&tasks, &id).await;
    let epoch = original.request_epoch.as_deref().expect("epoch binding");
    drop(
        tasks
            .acknowledge(&id, epoch, &original.submission_sha256)
            .expect("first acknowledgement"),
    );
    let recovered_ack = tasks
        .acknowledge(&id, epoch, &original.submission_sha256)
        .expect("lost acknowledgement is idempotent");
    assert!(recovered_ack.acknowledged);
    assert!(!recovered_ack.result_available);
    assert_eq!(recovered_ack.request, Some(original.clone()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&tasks.0) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request runner released journal ownership");
    drop(tasks);

    let recovered = RequestTasks::open(dir.path()).expect("restart journal owner");
    let mut retry = original;
    retry.server_instance_id = "restarted-server".into();
    recovered
        .submit("test", id.clone(), retry.clone(), || async {
            panic!("acknowledged request must not replay after restart")
        })
        .expect("recover retained acknowledgement");
    let ack = recovered
        .acknowledge(
            &id,
            retry.request_epoch.as_deref().expect("original epoch"),
            &retry.submission_sha256,
        )
        .expect("repeat lost acknowledgement after restart");
    assert!(ack.acknowledged);
    assert!(!ack.result_available);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_retry_after_journal_acceptance_failure_never_dispatches_the_failed_identity() {
    let dir = tempfile::tempdir().expect("request journal");
    let path = dir.path().join("journal");
    let moved = dir.path().join("temporarily-unavailable");
    let tasks = RequestTasks::open(&path).expect("open journal");
    let (id, original) = prepare_submission(&tasks);
    std::fs::rename(&path, &moved).expect("simulate unavailable journal directory");
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let tasks = tasks.clone();
        let id = id.clone();
        let original = original.clone();
        let barrier = Arc::clone(&barrier);
        let calls = Arc::clone(&calls);
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            tasks.submit("test", id.clone(), original, move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(CallToolResult::success(vec![]))
            })
        }));
    }
    barrier.wait().await;
    let mut rejection_count = 0;
    for attempt in attempts {
        match attempt.await.expect("concurrent submitter") {
            Err(error) => {
                assert_no_start(&error, "request_journal_unavailable");
                rejection_count += 1;
            }
            Ok(task) => assert_eq!(task.status, TaskStatus::Failed),
        }
    }
    assert!(rejection_count >= 1);
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    std::fs::rename(&moved, &path).expect("restore journal directory");
    tasks
        .submit("test", id.clone(), original, || async {
            panic!("failed acceptance must remain not started after journal recovery")
        })
        .expect("recover failed identity");
    assert_eq!(
        tasks.get_retained(&id).expect("no-start receipt").execution.disposition,
        Disposition::NotStarted
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
