//! Actual runtime-free child fixtures; no Lean runtime or external build.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "private process fixtures assert broker drain ownership and cancellation"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

const LIMIT: Duration = Duration::from_secs(3);
const WORKER: &str = r"#!/usr/bin/env python3
import os, signal, sys, time
from pathlib import Path
root = Path(__file__).parent
signal.alarm(5)
with (root / 'events').open('a') as events:
    events.write(sys.argv[1] + '\n')
if sys.argv[1] == '--lean-rs-ready-host-artifacts':
    if not (root / 'ready').exists():
        sys.exit(75)
elif sys.argv[1] == '--lean-rs-prepare-host-artifacts':
    # Publish readiness only after the PID file is complete.
    (root / 'preparing.tmp').write_text(str(os.getpid()))
    (root / 'preparing.tmp').replace(root / 'preparing')
    while not (root / 'go').exists():
        time.sleep(0.01)
    (root / 'ready').write_text('ready')
else:
    sys.exit(19)
os.write(1, (root / 'response').read_bytes())
";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    worker: PathBuf,
    sysroot: PathBuf,
    writer: PathBuf,
    artifacts: LeanWorkerPreparedHostArtifacts,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let worker = root.join("worker");
        fs::write(&worker, WORKER).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let sysroot = root.join("sysroot");
        fs::create_dir(&sysroot).unwrap();
        let writer = root.join("writer");
        fs::write(&writer, "test-owned writer").unwrap();
        let manifest = root.join("manifest.json");
        let manifest_json = "{\"fixture\":true}";
        fs::write(&manifest, manifest_json).unwrap();
        let artifacts = LeanWorkerPreparedHostArtifacts {
            protocol: 1,
            shim_manifest_path: manifest.to_string_lossy().into_owned(),
            shim_manifest_json: manifest_json.into(),
            artifact_roots: vec![sysroot.to_string_lossy().into_owned()],
            shim_search_paths: vec![sysroot.to_string_lossy().into_owned()],
        };
        fs::write(root.join("response"), serde_json::to_vec(&artifacts).unwrap()).unwrap();
        Self {
            _directory: directory,
            root,
            worker,
            sysroot,
            writer,
            artifacts,
        }
    }

    async fn prepare(
        &self,
        broker: &ProjectBroker,
        cancellation: &CallCancellation,
    ) -> Result<LeanWorkerPreparedHostArtifacts> {
        broker
            .prepare_host_artifacts(
                &self.root,
                &self.worker,
                &self.sysroot,
                &self.writer,
                tokio::time::Instant::now().checked_add(LIMIT).unwrap(),
                cancellation,
            )
            .await
    }

    fn preparation_count(&self) -> usize {
        fs::read_to_string(self.root.join("events"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "--lean-rs-prepare-host-artifacts")
            .count()
    }
}

fn broker() -> Arc<ProjectBroker> {
    ProjectBroker::new(super::super::BrokerConfig {
        env_default: None,
        cwd: PathBuf::from("/unused"),
        max_projects: std::num::NonZeroUsize::MIN,
        max_workers_per_project: std::num::NonZeroUsize::MIN,
        idle_timeout: Duration::ZERO,
    })
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(LIMIT, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture transition bounded");
}

#[tokio::test]
async fn cold_preparation_drains_without_own_opening_or_capacity_reservation() {
    let broker = broker();
    let fixture = Fixture::new();
    let earlier_slot = Arc::clone(&broker.worker_slots).acquire_owned().await.unwrap();
    let cancellation = CallCancellation::default();
    let prepare = fixture.prepare(&broker, &cancellation);
    tokio::pin!(prepare);
    tokio::select! {
        result = &mut prepare => panic!("preparation bypassed an existing slot: {result:?}"),
        () = wait_for(|| broker.runtime_control(&RuntimeControlRequest::default()).pool.admission_paused) => {},
    }
    let waiting = broker.runtime_control(&RuntimeControlRequest::default());
    assert!(!waiting.pool.workers_stopped);
    assert_eq!(waiting.pool.occupied_slots, 1);
    assert!(waiting.pool.acquiring_projects.is_empty());
    assert_eq!(fixture.preparation_count(), 0);
    drop(earlier_slot);
    fs::write(fixture.root.join("go"), "continue").unwrap();
    assert_eq!(prepare.await.unwrap(), fixture.artifacts);
    let complete = broker.runtime_control(&RuntimeControlRequest::default());
    assert!(complete.pool.workers_stopped && !complete.pool.admission_paused);
    assert_eq!(fixture.preparation_count(), 1);
}

#[tokio::test]
async fn concurrent_cold_callers_share_one_drain_and_never_duplicate_preparation() {
    let broker = broker();
    let fixture = Fixture::new();
    let cancellation = CallCancellation::default();
    let first = fixture.prepare(&broker, &cancellation);
    tokio::pin!(first);
    tokio::select! {
        result = &mut first => panic!("preparation returned before fixture release: {result:?}"),
        () = wait_for(|| fixture.root.join("preparing").exists()) => {},
    }
    let second = fixture.prepare(&broker, &CallCancellation::default()).await;
    assert!(matches!(second, Err(ServerError::WorkerUnavailable(info)) if info.reason == "worker_pool_draining"));
    assert_eq!(fixture.preparation_count(), 1);
    fs::write(fixture.root.join("go"), "continue").unwrap();
    assert_eq!(first.await.unwrap(), fixture.artifacts);
    assert_eq!(
        fixture.prepare(&broker, &CallCancellation::default()).await.unwrap(),
        fixture.artifacts
    );
    assert_eq!(fixture.preparation_count(), 1);
}

#[tokio::test]
async fn owned_drain_reprobes_readiness_before_starting_any_writer() {
    let broker = broker();
    let fixture = Fixture::new();
    let earlier_slot = Arc::clone(&broker.worker_slots).acquire_owned().await.unwrap();
    let cancellation = CallCancellation::default();
    let prepare = fixture.prepare(&broker, &cancellation);
    tokio::pin!(prepare);
    tokio::select! {
        result = &mut prepare => panic!("preparation bypassed an existing slot: {result:?}"),
        () = wait_for(|| broker.runtime_control(&RuntimeControlRequest::default()).pool.admission_paused) => {},
    }
    fs::write(fixture.root.join("ready"), "another preparer completed").unwrap();
    drop(earlier_slot);
    assert_eq!(prepare.await.unwrap(), fixture.artifacts);
    assert_eq!(fixture.preparation_count(), 0);
}

#[tokio::test]
async fn cancelled_preparation_stops_actual_child_releases_pause_and_does_not_replay() {
    let broker = broker();
    let fixture = Fixture::new();
    let request = tokio_util::sync::CancellationToken::new();
    let operation = crate::cancellation::with_cancellation_evidence(&request, async {
        fixture.prepare(&broker, &CallCancellation::current()).await
    });
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => panic!("preparation returned before cancellation: {result:?}"),
        () = wait_for(|| fixture.root.join("preparing").exists()) => {},
    }
    let pid: i32 = fs::read_to_string(fixture.root.join("preparing"))
        .unwrap()
        .parse()
        .unwrap();
    request.cancel();
    let (result, evidence) = operation.await;
    assert!(
        matches!(result, Err(ServerError::PreparationFailed { target_check, .. }) if target_check.stage == "host_artifact_preparation")
    );
    assert!(evidence.uncertain);
    assert_eq!(evidence.pending, 0);
    assert_eq!(evidence.target_check.unwrap().disposition, "not_started");
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    wait_for(|| rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)).await;
    assert!(
        !broker
            .runtime_control(&RuntimeControlRequest::default())
            .pool
            .admission_paused
    );
    assert_eq!(fixture.preparation_count(), 1);
    assert!(!fixture.root.join("ready").exists());
}

#[tokio::test]
async fn warm_preparation_probe_preserves_occupied_pool_and_has_no_drain() {
    let broker = broker();
    let fixture = Fixture::new();
    fs::write(fixture.root.join("ready"), "warm").unwrap();
    let _earlier_slot = Arc::clone(&broker.worker_slots).acquire_owned().await.unwrap();
    assert_eq!(
        fixture.prepare(&broker, &CallCancellation::default()).await.unwrap(),
        fixture.artifacts
    );
    let snapshot = broker.runtime_control(&RuntimeControlRequest::default());
    assert!(!snapshot.pool.admission_paused);
    assert_eq!(snapshot.pool.occupied_slots, 1);
    assert_eq!(fixture.preparation_count(), 0);
}

#[test]
fn preparation_guard_cannot_release_successor_lease() {
    let broker = broker();
    let first = broker.runtime_control(&RuntimeControlRequest {
        action: RuntimeAction::Drain,
        ..RuntimeControlRequest::default()
    });
    let guard = PreparationLease {
        broker: &broker,
        token: first.lease_id.unwrap(),
    };
    let _released = broker.runtime_control(&RuntimeControlRequest {
        action: RuntimeAction::Release,
        lease_id: Some(guard.token.clone()),
        ..RuntimeControlRequest::default()
    });
    let second = broker.runtime_control(&RuntimeControlRequest {
        action: RuntimeAction::Drain,
        ..RuntimeControlRequest::default()
    });
    let token = second.lease_id.unwrap();
    drop(guard);
    assert!(
        broker
            .runtime_control(&RuntimeControlRequest::default())
            .pool
            .admission_paused
    );
    assert_eq!(
        broker
            .runtime_control(&RuntimeControlRequest {
                action: RuntimeAction::Release,
                lease_id: Some(token),
                ..RuntimeControlRequest::default()
            })
            .status,
        RuntimeControlStatus::Ok
    );
}

#[tokio::test]
async fn dropped_preparation_waiter_cancels_owned_child_without_replay() {
    let broker = broker();
    let fixture = Fixture::new();
    let cancellation = CallCancellation::default();
    let mut prepare = Box::pin(fixture.prepare(&broker, &cancellation));
    tokio::select! {
        result = &mut prepare => panic!("preparation returned before abandonment: {result:?}"),
        () = wait_for(|| fixture.root.join("preparing").exists()) => {},
    }
    let pid: i32 = fs::read_to_string(fixture.root.join("preparing"))
        .unwrap()
        .parse()
        .unwrap();
    drop(prepare);
    assert!(cancellation.is_cancelled());
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    wait_for(|| rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)).await;
    assert!(
        !broker
            .runtime_control(&RuntimeControlRequest::default())
            .pool
            .admission_paused
    );
    assert_eq!(fixture.preparation_count(), 1);
    assert!(!fixture.root.join("ready").exists());
}

#[tokio::test(start_paused = true)]
async fn cold_preparation_deadline_releases_lease_without_cancelling_existing_work() {
    use std::future::{Future as _, poll_fn};

    let broker = broker();
    let directory = tempfile::tempdir().unwrap();
    let _earlier_slot = Arc::clone(&broker.worker_slots).acquire_owned().await.unwrap();
    let cancellation = CallCancellation::default();
    let budget = Duration::from_secs(1);
    let deadline = tokio::time::Instant::now().checked_add(budget).unwrap();
    // Exercise the drain after a cold readiness result. The helper's process
    // startup time cannot decide whether this test reaches lease acquisition.
    let drain = broker.preparation_drain(directory.path(), deadline, &cancellation);
    tokio::pin!(drain);
    poll_fn(|cx| {
        assert!(
            drain.as_mut().poll(cx).is_pending(),
            "drain must wait for the earlier slot"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert!(
        broker
            .runtime_control(&RuntimeControlRequest::default())
            .pool
            .admission_paused
    );
    tokio::time::advance(budget).await;
    let result = drain.await;
    assert!(matches!(result, Err(ServerError::WorkerUnavailable(info)) if info.reason == "project_admission_timeout"));
    let snapshot = broker.runtime_control(&RuntimeControlRequest::default());
    assert!(!snapshot.pool.admission_paused);
    assert_eq!(snapshot.pool.occupied_slots, 1);
    assert!(!cancellation.is_cancelled());
}

#[tokio::test]
async fn metadata_failure_before_any_helper_remains_provably_unstarted() {
    let broker = broker();
    let directory = tempfile::tempdir().unwrap();
    let request = tokio_util::sync::CancellationToken::new();
    let (result, evidence) =
        crate::cancellation::with_cancellation_evidence(&request, broker.acquire(directory.path().to_path_buf())).await;
    assert!(matches!(result, Err(ServerError::BadProject(_))));
    assert!(!evidence.uncertain);
    assert_eq!(evidence.pending, 0);
    assert_eq!(evidence.returned, 0);
}

#[test]
fn readiness_queued_past_admission_deadline_never_starts_helper() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("ready"), "already prepared").unwrap();
        for cancel in [false, true] {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (entered, occupied) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                let _ = wait.recv();
            });
            occupied.await.unwrap();
            let request = tokio_util::sync::CancellationToken::new();
            let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
                use std::future::{Future as _, poll_fn};
                use std::task::Poll;

                let cancellation = CallCancellation::current();
                let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
                let probe = run_helper(
                    &fixture.root,
                    &fixture.worker,
                    &fixture.sysroot,
                    &fixture.writer,
                    deadline,
                    &cancellation,
                    HelperMode::Ready,
                    None,
                );
                tokio::pin!(probe);
                // With no drain lease, the only pending await in run_helper is the
                // already-submitted blocking task. An entry-time refusal is Ready.
                poll_fn(|cx| {
                    assert!(
                        probe.as_mut().poll(cx).is_pending(),
                        "probe did not reach the blocking queue"
                    );
                    Poll::Ready(())
                })
                .await;
                tokio::time::sleep_until(deadline).await;
                if cancel {
                    cancellation.cancel();
                }
                release.send(()).unwrap();
                blocker.await.unwrap();
                probe.await
            })
            .await;
            let expected = if cancel {
                "request_cancelled"
            } else {
                "project_admission_timeout"
            };
            assert!(matches!(result, Err(ServerError::WorkerUnavailable(info)) if info.reason == expected));
            assert_eq!(evidence.not_started, 1);
            assert_eq!(evidence.pending, 0);
            assert_eq!(evidence.returned, 0);
            assert_eq!(evidence.stopped, 0);
            assert!(!evidence.uncertain);
            assert!(
                !fixture.root.join("events").exists(),
                "no readiness command reached the fixture"
            );
        }
    });
}
