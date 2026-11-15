//! Exercise host receipt propagation through an actual actor and framed child.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use lean_rs_worker_parent::{
    LeanWorker, LeanWorkerConfig, LeanWorkerProofCandidate, LeanWorkerProofEditTarget, LeanWorkerProofPositionSelector,
    LeanWorkerStatus,
};
use lean_rs_worker_protocol::protocol::{MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, Response, write_frame};
use lean_rs_worker_protocol::types::LeanWorkerImportStats;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::*;

const EXECUTION_TIMEOUT: Duration = Duration::from_millis(300);
const TEST_DEADLINE: Duration = Duration::from_secs(5);

/// One real protocol process per generation; no Lean or Lake installation is used.
const FAKE_WORKER: &str = r"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import struct
import sys
import time

root = Path(__file__).parent
output = sys.stdout.buffer
input = sys.stdin.buffer

def send(name):
    output.write((root / name).read_bytes())
    output.flush()

def receive():
    header = input.read(4)
    if not header:
        sys.exit(0)
    length = struct.unpack('>I', header)[0]
    return json.loads(input.read(length))['message']

with (root / 'requests').open('a') as log:
    log.write(json.dumps({'pid': os.getpid(), 'op': 'spawn'}) + '\n')
send('handshake')
receive() # ConfigureFrameLimit
while True:
    request = receive()['body']
    operation = request['op']
    with (root / 'requests').open('a') as log:
        log.write(json.dumps({'pid': os.getpid(), 'op': operation}) + '\n')
    if operation == 'health':
        if (root / 'hang_one_health').exists():
            (root / 'hang_one_health').unlink()
            while True:
                time.sleep(60)
        send('healthy')
    elif operation == 'open_host_session':
        send('session')
    elif operation == 'terminate':
        send('terminating')
        sys.exit(0)
    elif operation == 'attempt_proof':
        with (root / 'effects').open('a') as effects:
            effects.write('effect before abrupt exit\n')
        sys.exit(17)
    elif operation == 'process_module_query':
        if (root / 'remove_executable').exists():
            Path(__file__).unlink()
        while True:
            time.sleep(60)
    else:
        raise RuntimeError('unexpected request ' + operation)
";

fn write_message(root: &Path, name: &str, message: Message) {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, message, MAX_FRAME_BYTES).expect("encode fixture frame");
    fs::write(root.join(name), bytes).expect("write fixture frame");
}

fn fake_worker(root: &Path, remove_executable: bool) -> PathBuf {
    write_message(
        root,
        "handshake",
        Message::Handshake {
            worker_version: "host-receipt-test".into(),
            protocol_version: PROTOCOL_VERSION,
        },
    );
    write_message(root, "healthy", Message::Response(Response::HealthOk));
    write_message(root, "terminating", Message::Response(Response::Terminating));
    write_message(
        root,
        "session",
        Message::Response(Response::HostSessionOpened {
            import_stats: LeanWorkerImportStats {
                direct_import_names: Vec::new(),
                effective_module_count: 0,
                compacted_region_count: 0,
                memory_mapped_region_count: 0,
                compacted_region_bytes: 0,
                memory_mapped_region_bytes: 0,
                non_memory_mapped_region_bytes: 0,
                imported_bytes: 0,
                imported_constant_count: 0,
                extension_count: 0,
                total_imported_extension_entries: 0,
                import_level: "exported".into(),
                import_all: false,
                load_exts: true,
            },
        }),
    );
    if remove_executable {
        fs::write(root.join("remove_executable"), "").expect("configure failed replacement");
    }
    let executable = root.join("fake-worker");
    fs::write(&executable, FAKE_WORKER).expect("write framed fake child");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("make fake child executable");
    executable
}

fn fixture_project(root: &Path) -> Arc<LeanProject> {
    Arc::new(LeanProject {
        canonical_root: root.to_path_buf(),
        toolchain: "fixture".into(),
        package: None,
        library: None,
        manifest_hash: "fixture".into(),
        session_id: uuid::Uuid::new_v4().to_string(),
        open_warnings: Vec::new(),
        actor_tx: Mutex::new(None),
        admission_timeout: TEST_DEADLINE,
        active_jobs: Arc::new(AtomicUsize::new(0)),
        pool_active_jobs: Arc::new(AtomicUsize::new(0)),
        healthy: Arc::new(AtomicBool::new(true)),
        runtime: Arc::new(Mutex::new(RuntimeSnapshot {
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
    })
}

fn fixture_config(
    root: &Path,
    executable: PathBuf,
    project: &LeanProject,
    tracker: &LeanWorkerProcessTracker,
) -> ActorConfig {
    let defaults = ProjectRuntimeConfig::default();
    ActorConfig {
        maintenance: crate::broker::MaintenanceAccess::default(),
        lake_root: root.to_path_buf(),
        manifest_hash: "fixture".into(),
        toolchain_label: "fixture".into(),
        worker_path: executable,
        lean_sysroot: root.join("unused-sysroot"),
        session_id: project.session_id.clone(),
        runtime: Arc::clone(&project.runtime),
        healthy: Arc::clone(&project.healthy),
        startup_uncertain: Arc::new(AtomicBool::new(false)),
        process_tracker: tracker.clone(),
        lean_max_memory_kib: defaults.lean_max_memory_kib(),
        request_timeout_millis: millis_u64(EXECUTION_TIMEOUT),
        mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
        max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
        restart_window: RESTART_WINDOW,
        import_residue_budget_bytes: defaults.import_residue_budget_bytes(),
        import_residue_soft_bytes: defaults.import_residue_soft_bytes(),
        session_pool_capacity: WORKER_SESSION_POOL_CAPACITY,
        tokio_handle: None,
        toolchain_advisories: Vec::new(),
    }
}

fn requests_at(root: &Path, operation: &str) -> Vec<serde_json::Value> {
    fs::read_to_string(root.join("requests"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|request| request["op"] == operation)
        .collect()
}

fn ordinary_state(root: &Path, tracker: &LeanWorkerProcessTracker) -> (Arc<LeanProject>, ProjectActorState) {
    fs::create_dir_all(root).expect("ordinary worker directory");
    let project = fixture_project(root);
    let config = fixture_config(root, fake_worker(root, false), &project, tracker);
    let (handle, _) = open_worker(&config).expect("ordinary worker bootstrap");
    let state = ProjectActorState {
        runtime: Arc::clone(&config.runtime),
        config,
        handle,
        worker_generation_base: 1,
        last_restart: None,
        imports_seen: Vec::new(),
        profile_switch_count: 0,
        last_rss_kib: None,
        abnormal_restart_times: VecDeque::new(),
        restart_stats: RestartStats::default(),
        cycled_while_idle: false,
    };
    (project, state)
}

fn semantic_worker(root: &Path, tracker: &LeanWorkerProcessTracker, hang_one_health: bool) -> LeanWorker {
    fs::create_dir_all(root).expect("semantic worker directory");
    let executable = fake_worker(root, false);
    if hang_one_health {
        fs::write(root.join("hang_one_health"), "").expect("configure companion timeout");
    }
    LeanWorker::spawn(
        &LeanWorkerConfig::new(executable)
            .process_tracker(tracker.clone())
            .startup_timeout(TEST_DEADLINE)
            .shutdown_timeout(Duration::from_millis(20))
            .request_timeout(EXECUTION_TIMEOUT),
    )
    .expect("semantic protocol child handshake")
}

fn tracked_slot(tracker: &LeanWorkerProcessTracker) -> (Arc<Semaphore>, WorkerSlot) {
    let capacity = Arc::new(Semaphore::new(1));
    let permit = Arc::clone(&capacity).try_acquire_owned().expect("reserve shared slot");
    let slot = WorkerSlot::new(permit, tracker.clone());
    (capacity, slot)
}

async fn confirmed_capacity_release(capacity: &Semaphore) {
    tokio::time::timeout(TEST_DEADLINE, async {
        while capacity.available_permits() == 0 {
            crate::worker_retirement::poll_retirements();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("confirmed exits release shared capacity");
    assert_eq!(capacity.available_permits(), 1, "slot releases exactly once");
}

struct ActorHarness {
    root: tempfile::TempDir,
    project: Arc<LeanProject>,
    tracker: LeanWorkerProcessTracker,
    capacity: Arc<Semaphore>,
    actor: Option<thread::JoinHandle<()>>,
}

impl ActorHarness {
    fn start(remove_executable: bool) -> Self {
        let root = tempfile::tempdir().expect("private actor fixture");
        let executable = fake_worker(root.path(), remove_executable);
        let project = fixture_project(root.path());
        let tracker = LeanWorkerProcessTracker::new();
        let config = fixture_config(root.path(), executable, &project, &tracker);
        let capacity = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&capacity)
            .try_acquire_owned()
            .expect("reserve actor capacity");
        let (reply, receive) = std::sync::mpsc::channel();
        let actor = thread::spawn(move || {
            let _slot = WorkerSlot::new(permit, config.process_tracker.clone());
            actor_main(config, reply);
        });
        let (_, sender) = receive
            .recv_timeout(TEST_DEADLINE)
            .expect("bounded actor initialization")
            .expect("fake worker bootstrap health succeeds");
        *project.actor_tx.lock() = Some(sender);
        Self {
            root,
            project,
            tracker,
            capacity,
            actor: Some(actor),
        }
    }

    fn requests(&self, operation: &str) -> Vec<serde_json::Value> {
        requests_at(self.root.path(), operation)
    }

    async fn wait_for_request(&self) {
        tokio::time::timeout(TEST_DEADLINE, async {
            while self.requests("process_module_query").is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("typed query reaches child");
    }

    async fn wait_for_retirement(&self) {
        let actor = self.actor.as_ref().expect("owned actor thread");
        tokio::time::timeout(TEST_DEADLINE, async {
            while !actor.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failed worker retires its actor without caller intervention");
        crate::worker_retirement::poll_retirements();
        assert_eq!(self.tracker.active_processes(), 0);
        assert_eq!(self.capacity.available_permits(), 1);
    }

    async fn finish(mut self) {
        self.project.shutdown();
        let actor = self.actor.take().expect("actor thread");
        tokio::time::timeout(TEST_DEADLINE, async {
            while !actor.is_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("bounded actor drain");
        actor.join().expect("actor did not panic");
        crate::worker_retirement::poll_retirements();
        assert_eq!(self.tracker.active_processes(), 0, "all owned generations reaped");
        assert_eq!(self.capacity.available_permits(), 1, "drained actor releases capacity");
    }
}

impl Drop for ActorHarness {
    fn drop(&mut self) {
        self.project.shutdown();
    }
}

async fn query(project: &LeanProject) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
    project
        .process_module_query(
            Vec::new(),
            "theorem fixture : True := True.intro".into(),
            LeanWorkerModuleQuery::Diagnostics,
            LeanWorkerElabOptions::default(),
        )
        .await
}

/// A retained-request test can run this inside its own cancellation scope and
/// observe genuine actor evidence without depending on an installed Lean worker.
pub(crate) async fn retained_timeout_call(
    remove_executable: bool,
) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
    let harness = ActorHarness::start(remove_executable);
    let result = query(&harness.project).await;
    assert_eq!(harness.requests("process_module_query").len(), 1);
    harness.finish().await;
    result
}

/// Cancel after any caller-owned preparation, but before this actor can receive
/// a source operation. Preparation effects remain the caller's own evidence.
pub(crate) async fn retained_cancelled_before_dispatch_call() -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
    let harness = ActorHarness::start(false);
    CallCancellation::current().cancel();
    let result = query(&harness.project).await;
    assert!(harness.requests("process_module_query").is_empty());
    harness.finish().await;
    result
}

fn failure(error: ServerError) -> Box<WorkerUnavailable> {
    if let ServerError::WorkerUnavailable(failure) = error {
        failure
    } else {
        panic!("expected worker_unavailable carrying real stop evidence: {error:?}");
    }
}

fn assert_stopped_call(
    failure: &WorkerUnavailable,
    evidence: &crate::cancellation::CallExecutionEvidence,
    harness: &ActorHarness,
) {
    let receipt = failure
        .worker_stop_receipt
        .as_ref()
        .expect("upstream-confirmed request stop");
    assert_eq!(receipt.worker_session_id, harness.project.session_id);
    assert_eq!(receipt.worker_generation, 1, "receipt names interrupted generation");
    assert_eq!(receipt.operation, "worker_process_module_query");
    assert_eq!(receipt.worker_scope, "main");
    assert!(receipt.worker_request_id.is_some());
    assert_eq!(receipt.exit_success, Some(false));
    assert!(receipt.exit_status.as_ref().is_some_and(|status| !status.is_empty()));
    assert_eq!(
        receipt.worker_pid.map(u64::from),
        harness.requests("process_module_query")[0]["pid"].as_u64(),
        "receipt names the process that actually accepted this query"
    );
    assert_eq!(failure.runtime.retry_count, 0, "host never replays timed-out source");
    assert_eq!(failure.runtime.final_attempt, FinalAttemptStatus::Interrupted);
    assert_eq!(evidence.pending, 0);
    assert_eq!(evidence.returned, 0, "timeout is never reported as a result");
    assert_eq!(evidence.stopped, 1);
    assert_eq!(evidence.worker_stop_receipt.as_ref(), Some(receipt));
    assert_eq!(harness.requests("process_module_query").len(), 1);
}

#[tokio::test]
async fn actor_timeout_preserves_direct_worker_receipt_without_replay() {
    let harness = ActorHarness::start(false);
    let (result, evidence) =
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&harness.project)).await;
    let failure = failure(result.expect_err("source query must time out"));
    assert_stopped_call(&failure, &evidence, &harness);
    assert!(!evidence.uncertain);
    assert!(failure.worker_restarted);
    assert!(harness.project.is_healthy(), "successful replacement remains warm");
    assert_eq!(harness.requests("spawn").len(), 2, "one replacement generation");
    assert_eq!(harness.tracker.active_processes(), 1, "replacement still owns capacity");
    assert_eq!(harness.capacity.available_permits(), 0);
    harness.finish().await;
}

#[tokio::test]
async fn actor_failed_replacement_keeps_original_stop_without_replay() {
    let harness = ActorHarness::start(true);
    let (result, evidence) =
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&harness.project)).await;
    let failure = failure(result.expect_err("replacement executable was removed"));
    assert_stopped_call(&failure, &evidence, &harness);
    assert!(!evidence.uncertain);
    assert_eq!(harness.requests("spawn").len(), 1, "replacement did not start");
    assert_eq!(harness.tracker.active_processes(), 0, "original child reaped");
    assert!(!harness.project.is_healthy(), "failed spawn seals the dead actor");
    harness.wait_for_retirement().await;
    let (fresh, fresh_evidence) =
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&harness.project)).await;
    assert!(fresh.is_err(), "a fresh call needs a new actor");
    assert_eq!(fresh_evidence.not_started, 1);
    assert!(!fresh_evidence.uncertain);
    assert_eq!(harness.requests("process_module_query").len(), 1);
    assert_eq!(
        harness.requests("spawn").len(),
        1,
        "the failed request is never replayed"
    );
    harness.finish().await;
}

#[tokio::test]
async fn abrupt_worker_exit_after_effect_never_replays_actual_proof_attempt() {
    let harness = ActorHarness::start(false);
    let (result, evidence) = crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), async {
        harness
            .project
            .attempt_proof(
                Vec::new(),
                LeanWorkerProofAttemptRequest {
                    source: "theorem fixture : True := by trivial".into(),
                    edit: LeanWorkerProofEditTarget::Declaration {
                        name: "fixture".into(),
                        position: LeanWorkerProofPositionSelector::default(),
                    },
                    candidates: vec![LeanWorkerProofCandidate {
                        id: "one".into(),
                        text: "trivial".into(),
                    }],
                    budgets: LeanWorkerOutputBudgets::default(),
                },
                LeanWorkerElabOptions::default(),
            )
            .await
    })
    .await;
    let failure = failure(result.expect_err("child exits after recording its effect"));
    let receipt = failure.worker_stop_receipt.as_ref().expect("actual child exit receipt");
    assert_eq!(receipt.operation, "worker_attempt_proof");
    assert_eq!(receipt.worker_scope, "main");
    assert_eq!(receipt.exit_success, Some(false));
    assert_eq!(receipt.exit_code, Some(17));
    assert_eq!(receipt.worker_session_id, harness.project.session_id);
    assert_eq!(
        receipt.worker_pid.map(u64::from),
        harness.requests("attempt_proof")[0]["pid"].as_u64()
    );
    assert_eq!(evidence.stopped, 1);
    assert_eq!(evidence.returned, 0);
    assert!(!evidence.uncertain);
    assert_eq!(failure.runtime.retry_count, 0);
    assert!(!harness.project.is_healthy(), "observed child death seals its actor");
    harness.wait_for_retirement().await;
    assert_eq!(
        harness.requests("attempt_proof").len(),
        1,
        "source command is delivered once"
    );
    assert_eq!(
        fs::read_to_string(harness.root.path().join("effects")).unwrap(),
        "effect before abrupt exit\n",
        "an exit witness cannot justify replaying earlier effects"
    );
    harness.finish().await;
}

#[tokio::test]
async fn prior_companion_uncertainty_survives_main_worker_stop_receipt() {
    let harness = ActorHarness::start(false);
    let (result, evidence) = crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), async {
        // This is the aggregate marker used when semantic companion preparation
        // has an unresolved outcome. The main call below still runs a real child.
        CallCancellation::current().mark_uncertain();
        query(&harness.project).await
    })
    .await;
    let failure = failure(result.expect_err("main source query must time out"));
    assert_stopped_call(&failure, &evidence, &harness);
    assert!(
        evidence.uncertain,
        "main receipt cannot settle another process's unknown outcome"
    );
    harness.finish().await;
}

#[tokio::test]
async fn drain_during_timeout_retains_capacity_until_actor_and_children_exit() {
    let harness = ActorHarness::start(false);
    let project = Arc::clone(&harness.project);
    let call = tokio::spawn(async move {
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&project)).await
    });
    harness.wait_for_request().await;
    harness.project.shutdown();
    crate::worker_retirement::poll_retirements();
    assert_eq!(
        harness.capacity.available_permits(),
        0,
        "active actor still owns its slot"
    );
    let (result, evidence) = tokio::time::timeout(TEST_DEADLINE, call)
        .await
        .expect("bounded timeout cleanup")
        .expect("caller did not panic");
    let failure = failure(result.expect_err("drain cannot manufacture a successful query"));
    assert_stopped_call(&failure, &evidence, &harness);
    assert!(!evidence.uncertain);
    harness.finish().await;
}

#[tokio::test]
async fn semantic_shutdown_receipt_cannot_settle_ordinary_or_new_companion_scope() {
    let root = tempfile::tempdir().expect("separate worker scopes");
    let tracker = LeanWorkerProcessTracker::new();
    let (capacity, slot) = tracked_slot(&tracker);
    let main_root = root.path().join("main");
    let (project, mut state) = ordinary_state(&main_root, &tracker);
    let main_checkpoint = state.handle.request_checkpoint();
    let mut companion = semantic_worker(&root.path().join("semantic"), &tracker, false);
    let companion_checkpoint = companion.request_checkpoint();
    let companion_pid = companion.__child_pid_for_test().expect("owned companion");
    assert_eq!(tracker.active_processes(), 2);
    companion.health().expect("real companion request");
    let shutdown = companion.try_shutdown();
    let after = companion.lifecycle_snapshot();
    let upstream = companion
        .confirmed_request_stop_since(&companion_checkpoint)
        .expect("genuine companion request stop")
        .clone();
    let meta = project.job_meta(Vec::new(), RetryPolicy::Never);
    let receipt = state
        .finish_semantic_shutdown(&meta, &after, shutdown, Some(&upstream))
        .expect("same production adapter confirms shutdown")
        .expect("semantic receipt");
    assert_eq!(receipt.worker_scope, "semantic");
    assert_eq!(receipt.worker_pid, Some(companion_pid));
    assert_eq!(receipt.worker_session_id, project.session_id);
    assert_eq!(receipt.operation_id, meta.correlation_id.to_string());
    assert_eq!(receipt.operation, "health");
    assert_eq!(receipt.exit_success, Some(true));
    assert_ne!(
        receipt.worker_pid.map(u64::from),
        requests_at(&main_root, "health")[0]["pid"].as_u64()
    );
    assert!(
        state
            .handle
            .confirmed_request_stop_since(&companion_checkpoint)
            .is_none()
    );
    assert!(companion.confirmed_request_stop_since(&main_checkpoint).is_none());
    assert!(matches!(state.handle.status().unwrap(), LeanWorkerStatus::Running));
    assert_eq!(tracker.active_processes(), 1, "ordinary worker remains owned");
    assert_eq!(capacity.available_permits(), 0);

    let mut next_companion = semantic_worker(&root.path().join("next-semantic"), &tracker, false);
    let next_checkpoint = next_companion.request_checkpoint();
    assert!(companion.confirmed_request_stop_since(&next_checkpoint).is_none());
    assert!(
        next_companion
            .confirmed_request_stop_since(&companion_checkpoint)
            .is_none()
    );
    assert_eq!(tracker.active_processes(), 2);
    state.config.healthy.store(false, Ordering::Release);
    state.handle.try_shutdown().expect("reap ordinary worker");
    assert_eq!(tracker.active_processes(), 1, "new companion still owns capacity");
    drop(slot);
    crate::worker_retirement::poll_retirements();
    assert_eq!(
        capacity.available_permits(),
        0,
        "old receipt cannot release a new child"
    );
    next_companion.try_shutdown().expect("reap new companion");
    assert_eq!(tracker.active_processes(), 0);
    confirmed_capacity_release(&capacity).await;
}

#[tokio::test]
async fn injected_semantic_cleanup_failure_keeps_original_receipt_and_live_replacement_unknown() {
    let root = tempfile::tempdir().expect("companion replacement scopes");
    let tracker = LeanWorkerProcessTracker::new();
    let (capacity, slot) = tracked_slot(&tracker);
    let (project, mut state) = ordinary_state(&root.path().join("main"), &tracker);
    let main_checkpoint = state.handle.request_checkpoint();
    let mut companion = semantic_worker(&root.path().join("semantic"), &tracker, true);
    let checkpoint = companion.request_checkpoint();
    let original_pid = companion.__child_pid_for_test().expect("original semantic child");
    assert!(matches!(companion.health(), Err(LeanWorkerError::Timeout { .. })));
    let upstream = companion
        .confirmed_request_stop_since(&checkpoint)
        .expect("actual timeout killed and reaped original companion")
        .clone();
    let replacement_pid = companion.__child_pid_for_test().expect("live replacement");
    assert_ne!(replacement_pid, original_pid);
    assert_eq!(upstream.process_id(), original_pid);
    assert_eq!(
        tracker.active_processes(),
        2,
        "ordinary worker plus semantic replacement"
    );
    assert!(companion.confirmed_request_stop_since(&main_checkpoint).is_none());
    assert!(state.handle.confirmed_request_stop_since(&checkpoint).is_none());
    assert!(
        companion
            .confirmed_request_stop_since(&companion.request_checkpoint())
            .is_none()
    );
    let after = companion.lifecycle_snapshot();

    let (error, evidence) = crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), async {
        let meta = project.job_meta(Vec::new(), RetryPolicy::Never);
        let operation = meta.cancellation.begin_operation();
        // Fault-inject only the cleanup syscall outcome. Both processes and the
        // interrupted request receipt are genuine; replacement stays live here.
        let error = state
            .finish_semantic_shutdown(
                &meta,
                &after,
                Err(LeanWorkerError::WaitTimeout {
                    operation: "injected_semantic_cleanup",
                    duration: Duration::ZERO,
                }),
                Some(&upstream),
            )
            .expect_err("cleanup failure remains separate from original request stop");
        let failure = failure(error);
        let receipt = failure
            .worker_stop_receipt
            .as_ref()
            .expect("original receipt preserved");
        assert_eq!(receipt.worker_scope, "semantic");
        assert_eq!(receipt.worker_pid, Some(original_pid));
        assert_ne!(receipt.worker_pid, Some(replacement_pid));
        assert_eq!(receipt.operation_id, meta.correlation_id.to_string());
        operation.stopped(receipt.clone());
        failure
    })
    .await;
    assert!(error.reason.contains("shutdown uncertain"));
    assert_eq!(evidence.stopped, 1, "original operation has real stop evidence");
    assert!(evidence.uncertain, "original stop cannot settle replacement cleanup");
    assert!(state.config.startup_uncertain.load(Ordering::Acquire));
    assert!(!state.config.healthy.load(Ordering::Acquire));
    assert!(matches!(state.handle.status().unwrap(), LeanWorkerStatus::Running));
    assert!(matches!(companion.status().unwrap(), LeanWorkerStatus::Running));
    drop(slot);
    crate::worker_retirement::poll_retirements();
    assert_eq!(capacity.available_permits(), 0);
    state.handle.try_shutdown().expect("actual ordinary worker shutdown");
    crate::worker_retirement::poll_retirements();
    assert_eq!(tracker.active_processes(), 1);
    assert_eq!(
        capacity.available_permits(),
        0,
        "semantic replacement still owns capacity"
    );
    companion.try_shutdown().expect("actual semantic replacement shutdown");
    assert_eq!(tracker.active_processes(), 0);
    confirmed_capacity_release(&capacity).await;
}
