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
};
use lean_rs_worker_protocol::protocol::{MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, Response, write_frame};
use lean_rs_worker_protocol::types::LeanWorkerImportStats;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cancellation::with_cancellation_evidence;

const EXECUTION_TIMEOUT: Duration = Duration::from_millis(300);
const TEST_DEADLINE: Duration = Duration::from_secs(5);

fn admission_deadline() -> Instant {
    Instant::now()
        .checked_add(TEST_DEADLINE)
        .expect("finite test admission")
}

/// One real protocol process per generation; no Lean or Lake installation is used.
const FAKE_WORKER: &str = r"#!/usr/bin/env python3
import json
import os
from pathlib import Path
import select
import struct
import sys
import time

root = Path(__file__).parent
output = sys.stdout.buffer
input = sys.stdin.buffer.raw

def send(name):
    output.write((root / name).read_bytes())
    output.flush()

def read_exact(size):
    result = bytearray()
    while len(result) < size:
        chunk = input.read(size - len(result))
        if not chunk:
            sys.exit(0)
        result.extend(chunk)
    return result

def receive():
    while not select.select([input], [], [], 0.01)[0]:
        if (root / 'idle_exit').exists():
            (root / 'idle_exited').write_text('exited')
            sys.exit(23)
    header = read_exact(4)
    length = struct.unpack('>I', header)[0]
    return json.loads(read_exact(length))['message']

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
        send('healthy')
    elif operation == 'open_host_session':
        if (root / 'change_artifact_epoch_once').exists():
            (root / 'change_artifact_epoch_once').unlink()
            send('artifact_epoch_changed')
        elif (root / 'require_missing_import').exists() and not (root / '.lake/build/lib/lean/B.olean').exists():
            send('missing_import')
        else:
            send('session')
    elif operation == 'host_runtime_metadata':
        while (root / 'hold_runtime_metadata').exists():
            time.sleep(0.005)
        send('metadata')
    elif operation == 'terminate':
        send('terminating')
        sys.exit(0)
    elif operation == 'attempt_proof':
        with (root / 'effects').open('a') as effects:
            effects.write('effect before abrupt exit\n')
        sys.exit(17)
    elif operation == 'process_module_query':
        if (root / 'complete_module_query').exists():
            with (root / 'query_effects').open('a') as effects:
                effects.write(str(os.getpid()) + '\n')
            send('query_completed')
            continue
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
        "artifact_epoch_changed",
        Message::Response(Response::HostArtifactEpochChanged),
    );
    write_message(
        root,
        "query_completed",
        Message::Response(Response::ProcessModuleQuery {
            outcome: LeanWorkerModuleQueryOutcome::Ok {
                imports: Vec::new(),
                result: lean_rs_worker_parent::LeanWorkerModuleQueryResult::Diagnostics(
                    lean_rs_worker_parent::LeanWorkerElabFailure {
                        diagnostics: Vec::new(),
                        truncated: false,
                    },
                ),
            },
        }),
    );
    write_message(
        root,
        "missing_import",
        Message::Response(Response::Error {
            code: "fixture.missing_import".into(),
            message: "object file 'B.olean' of module B does not exist".into(),
        }),
    );
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
    let expectation = crate::runtime_compatibility::session_expectation("leanprover/lean4:v4.35.0", None)
        .expect("supported fixture toolchain");
    write_message(
        root,
        "metadata",
        Message::Response(Response::HostRuntimeMetadata {
            metadata: lean_rs_worker_parent::LeanWorkerHostRuntimeMetadata {
                compiled_lean_version: "4.35.0".into(),
                compiled_header_sha256: expectation.compiled_header_sha256.expect("fixture header"),
                runtime_lean_version: Some("4.35.0".into()),
                checked_host_bindings: expectation.checked_host_bindings,
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
        toolchain: "leanprover/lean4:v4.35.0".into(),
        manifest_hash: "fixture".into(),
        session_id: uuid::Uuid::new_v4().to_string(),
        open_warnings: Vec::new(),
        actor_tx: Mutex::new(None),
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
            restart_retry_after: None,
            child_exited: false,
            compatibility: None,
        })),
        module_queries: ModuleQueryCache::with_capacity(NonZeroUsize::MIN),
        actor_scope: std::sync::Weak::new(),
        process_tracker: LeanWorkerProcessTracker::new(),
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
        toolchain_label: "leanprover/lean4:v4.35.0".into(),
        worker_path: executable,
        lean_sysroot: root.join("unused-sysroot"),
        prepared_host_artifacts: None,
        session_id: project.session_id.clone(),
        runtime: Arc::clone(&project.runtime),
        healthy: Arc::clone(&project.healthy),
        startup_uncertain: Arc::new(AtomicBool::new(false)),
        process_tracker: tracker.clone(),
        lean_max_memory_kib: defaults.lean_max_memory_kib(),
        request_timeout_millis: millis_u64(EXECUTION_TIMEOUT),
        mailbox_capacity: defaults.mailbox_capacity(),
        max_restarts_per_window: defaults.max_restarts_per_window(),
        restart_window: defaults.restart_window(),
        import_residue_budget_bytes: defaults.import_residue_budget_bytes(),
        import_residue_soft_bytes: defaults.import_residue_soft_bytes(),
        session_pool_capacity: defaults.session_pool_capacity(),
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
        loaded_at: None,
        profile_switch_count: 0,
        last_rss_kib: None,
        restart_stats: RestartStats::default(),
        cycled_while_idle: false,
    };
    (project, state)
}

fn tracked_worker(root: &Path, tracker: &LeanWorkerProcessTracker) -> LeanWorker {
    fs::create_dir_all(root).expect("tracked worker directory");
    let executable = fake_worker(root, false);
    LeanWorker::spawn(
        &LeanWorkerConfig::new(executable)
            .process_tracker(tracker.clone())
            .startup_timeout(TEST_DEADLINE)
            .shutdown_timeout(Duration::from_millis(20))
            .request_timeout(EXECUTION_TIMEOUT),
    )
    .expect("tracked protocol child handshake")
}

struct ActorHarness {
    broker: Arc<crate::broker::ProjectBroker>,
    root: tempfile::TempDir,
    project: Arc<LeanProject>,
    tracker: LeanWorkerProcessTracker,
    capacity: Arc<Semaphore>,
    actor: Option<thread::JoinHandle<()>>,
}

impl ActorHarness {
    fn start(remove_executable: bool) -> Self {
        Self::start_with_runtime(remove_executable, None, None, None)
    }

    fn start_with_runtime(
        remove_executable: bool,
        tokio_handle: Option<tokio::runtime::Handle>,
        soft_budget: Option<u64>,
        admission_timeout: Option<Duration>,
    ) -> Self {
        let root = tempfile::tempdir().expect("private actor fixture");
        let executable = fake_worker(root.path(), remove_executable);
        let broker_config = crate::broker::BrokerConfig {
            max_workers: NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
            ..crate::broker::BrokerConfig::serving(root.path().to_path_buf())
        };
        let mut runtime_config = ProjectRuntimeConfig::default();
        if let Some(timeout) = admission_timeout {
            runtime_config.request_timeout_millis = millis_u64(timeout);
        }
        let broker = crate::broker::ProjectBroker::new_with_runtime_config(broker_config, runtime_config);
        let maintenance = broker.test_maintenance_access();
        let mut project = fixture_project(root.path());
        let fixture = Arc::get_mut(&mut project).expect("private fixture project");
        fixture.pool_active_jobs = maintenance.activity_counter();
        if admission_timeout.is_some() {
            fs::write(root.path().join("lakefile.lean"), "package fixture\n").expect("private Lake metadata");
            fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").expect("private pin");
            fs::write(root.path().join("lake-manifest.json"), "{\"packages\":[]}\n").expect("private manifest");
            let meta = LakeProjectMeta::from_explicit(root.path()).expect("read metadata without running Lake");
            fixture.canonical_root = meta.canonical_root;
            fixture.manifest_hash = meta.manifest_hash;
        }
        broker.test_register_actor(Arc::clone(&project));
        let tracker = LeanWorkerProcessTracker::new();
        let mut config = fixture_config(root.path(), executable, &project, &tracker);
        config.maintenance = maintenance;
        config.tokio_handle = tokio_handle;
        if admission_timeout.is_some() {
            config.request_timeout_millis = millis_u64(TEST_DEADLINE);
        }
        if let Some(soft_budget) = soft_budget {
            config.import_residue_soft_bytes = soft_budget;
        }
        let capacity = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&capacity)
            .try_acquire_owned()
            .expect("reserve actor capacity");
        let (reply, receive) = std::sync::mpsc::channel();
        let actor = thread::spawn(move || {
            let _slot = WorkerSlot::new(permit, config.process_tracker.clone());
            actor_main(config, reply, CallCancellation::default().begin_operation());
        });
        let (_, sender) = receive
            .recv_timeout(TEST_DEADLINE)
            .expect("bounded actor initialization")
            .expect("fake worker bootstrap health succeeds");
        *project.actor_tx.lock() = Some(sender);
        Self {
            broker,
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
            while !actor.is_finished() || self.capacity.available_permits() != 1 {
                crate::worker_retirement::poll_retirements();
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
            admission_deadline(),
        )
        .await
}

/// A call that opens one host session and sends no target body.
async fn open_session(
    project: &LeanProject,
    deadline: Instant,
) -> Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>> {
    project.search_declarations(Vec::new(), Vec::new(), deadline).await
}

/// One refused warm open replaces the worker before the target body, and the
/// body runs once, on the replacement. This tests pre-body generation cycling,
/// not Lean elaboration, which has separate real-worker coverage.
#[tokio::test]
async fn one_warm_prebody_cycle_runs_the_target_body_once() {
    let harness = ActorHarness::start(false);
    open_session(&harness.project, admission_deadline())
        .await
        .expect("warm first protocol generation without executing a target");
    let old_pid = harness.requests("open_host_session")[0]["pid"].clone();
    assert!(harness.requests("process_module_query").is_empty());
    fs::write(harness.root.path().join("complete_module_query"), "").expect("complete one target body");
    fs::write(harness.root.path().join("change_artifact_epoch_once"), "").expect("refuse the warm pre-body open once");

    let result = query(&harness.project).await;
    let call = result
        .as_ref()
        .expect("one pre-body cycle resumes the original actor call");
    assert!(matches!(call.value, LeanWorkerModuleQueryOutcome::Ok { .. }));
    assert_eq!(call.runtime.worker_generation, 2);
    assert!(call.runtime.worker_restarted);
    assert_eq!(call.runtime.retry_count, 0, "the target body was never retried");
    assert_eq!(call.runtime.final_attempt, FinalAttemptStatus::Completed);
    let opens = harness.requests("open_host_session");
    assert_eq!(opens.len(), 3, "warm open, refused pre-body open, replacement open");
    assert_eq!(opens[1]["pid"], old_pid);
    assert_ne!(opens[2]["pid"], old_pid);
    let bodies = harness.requests("process_module_query");
    assert_eq!(bodies.len(), 1, "only the replacement receives a target body");
    assert_eq!(bodies[0]["pid"], opens[2]["pid"]);
    assert_eq!(
        fs::read_to_string(harness.root.path().join("query_effects")).expect("one target effect"),
        format!("{}\n", bodies[0]["pid"]),
    );
    let stopped = harness.requests("terminate");
    assert_eq!(
        stopped.len(),
        1,
        "old generation stopped before returning the target result"
    );
    assert_eq!(stopped[0]["pid"], old_pid);
    assert_eq!(harness.requests("spawn").len(), 2, "bounded one-generation cycle");
    assert_eq!(
        harness.tracker.active_processes(),
        1,
        "old child is reaped; only replacement remains"
    );
    assert!(harness.requests("attempt_proof").is_empty());
    let evidence = harness
        .broker
        .worker_compatibility(&harness.project.canonical_root)
        .expect("compiled contract")
        .expect("the completed call left a receipt");
    assert_eq!(
        evidence.loaded[0].worker_generation, 2,
        "the receipt names the replacement"
    );
    harness.finish().await;
}

/// A call cancelled before the actor receives it, as when its client goes
/// away while it waits, never sends its target body.
#[tokio::test]
async fn a_call_cancelled_before_dispatch_never_reaches_the_worker() {
    let harness = ActorHarness::start(false);
    let result = crate::cancellation::with_cancellation(&CancellationToken::new(), async {
        CallCancellation::current().cancel();
        query(&harness.project).await
    })
    .await;
    assert!(result.is_err(), "a cancelled call returns no result");
    assert!(harness.requests("process_module_query").is_empty());
    harness.finish().await;
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
async fn immediate_or_past_admission_deadline_sends_no_frame_to_an_idle_worker() {
    let harness = ActorHarness::start(false);
    for deadline in [
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("past deadline"),
        Instant::now(),
    ] {
        let result = open_session(&harness.project, deadline).await;
        assert!(matches!(result, Err(ServerError::WorkerUnavailable(info))
            if matches!(info.reason.as_str(), "queue_wait_timeout" | "mailbox_admission_timeout")));
        assert!(harness.requests("open_host_session").is_empty());
        assert_eq!(harness.capacity.available_permits(), 0);
    }
    open_session(&harness.project, admission_deadline())
        .await
        .expect("idle worker remains usable");
    assert_eq!(harness.requests("open_host_session").len(), 1);
    assert_eq!(harness.requests("spawn").len(), 1);
    harness.finish().await;
}

#[tokio::test]
async fn the_actor_queue_keeps_the_original_admission_deadline() {
    use crate::broker::ProjectHint;

    let admission_budget = Duration::from_secs(1);
    let harness = ActorHarness::start_with_runtime(false, None, None, Some(admission_budget));
    fs::write(harness.root.path().join("hold_runtime_metadata"), "").expect("hold the active request");
    let mut active = Box::pin(open_session(&harness.project, admission_deadline()));
    super::tests::poll_pending(active.as_mut()).await;
    tokio::time::timeout(TEST_DEADLINE, async {
        while harness.requests("host_runtime_metadata").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("active request reached its causal hold");
    // Inspect the actual broker-to-actor message without adding a production hook.
    let (probe_tx, mut probe_rx) = mpsc::channel(1);
    let actor_tx = harness
        .project
        .actor_tx
        .lock()
        .replace(probe_tx)
        .expect("retain the actual actor sender");
    let hint = ProjectHint::Explicit(harness.project.canonical_root.clone());
    let mut waiting = Box::pin(
        harness
            .broker
            .search_declarations(hint.clone(), Vec::new(), Vec::new(), Vec::new()),
    );
    super::tests::poll_pending(waiting.as_mut()).await;
    // The broker created its deadline during that poll. This ceiling stays
    // anchored there, even if observing the later handoff is delayed.
    let observed_admission = Instant::now();
    let original_deadline_ceiling = observed_admission
        .checked_add(admission_budget)
        .expect("finite original admission budget");
    let message = tokio::time::timeout_at(original_deadline_ceiling.into(), async {
        tokio::select! {
            biased;
            message = probe_rx.recv() => message.expect("the original queued message"),
            result = &mut waiting => panic!("request must reach the held actor before expiry: {result:?}"),
        }
    })
    .await
    .expect("waiter moved from broker admission to the actor mailbox");
    let queue_deadline = message.meta().queue.deadline;
    eprintln!(
        "admission handoff: elapsed_ms={}, original_remaining_ms={}, queue_remaining_ms={}",
        observed_admission.elapsed().as_millis(),
        original_deadline_ceiling
            .saturating_duration_since(Instant::now())
            .as_millis(),
        queue_deadline.saturating_duration_since(Instant::now()).as_millis(),
    );
    assert!(
        queue_deadline <= original_deadline_ceiling,
        "actor queue extended the original admission deadline by {:?}",
        queue_deadline.saturating_duration_since(original_deadline_ceiling),
    );
    actor_tx
        .send(message)
        .await
        .expect("forward the same message to the held real actor");
    *harness.project.actor_tx.lock() = Some(actor_tx);
    // This is only a liveness bound; the exact stored-deadline check above is
    // the reset-budget oracle, independent of observer scheduling.
    let result = tokio::time::timeout_at(original_deadline_ceiling.into(), &mut waiting)
        .await
        .expect("queued request expires at the original admission deadline");
    assert!(matches!(result, Err(ServerError::WorkerUnavailable(info)) if info.reason == "queue_wait_timeout"));
    assert_eq!(harness.requests("open_host_session").len(), 1);
    assert_eq!(harness.requests("host_runtime_metadata").len(), 1);
    assert_eq!(
        harness.capacity.available_permits(),
        0,
        "queued expiry releases no actor-owned capacity"
    );
    assert_eq!(harness.tracker.active_processes(), 1);

    fs::remove_file(harness.root.path().join("hold_runtime_metadata")).expect("release the active request");
    active.await.expect("queued expiry does not cancel the active request");
    harness
        .broker
        .search_declarations(hint, Vec::new(), Vec::new(), Vec::new())
        .await
        .expect("warm actor remains reusable");
    let opens = harness.requests("open_host_session");
    assert_eq!(
        opens.len(),
        2,
        "expired queued request sends no frame, even after actor release"
    );
    assert_eq!(opens[0]["pid"], opens[1]["pid"], "reuse the same warm generation");
    assert_eq!(harness.capacity.available_permits(), 0);
    drop(waiting);
    harness.finish().await;
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
    // The supervisor tries again at the next request. A new actor would bring
    // a new supervisor whose restart window starts empty, so this one stays.
    assert!(
        harness.project.is_healthy(),
        "a failed replacement keeps its supervisor"
    );
    let (fresh, fresh_evidence) =
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&harness.project)).await;
    assert!(fresh.is_err(), "the replacement still cannot start");
    assert_eq!(fresh_evidence.not_started, 1, "the fresh call reached no child");
    assert!(!fresh_evidence.uncertain);
    assert_eq!(harness.requests("process_module_query").len(), 1);
    assert_eq!(
        harness.requests("spawn").len(),
        1,
        "the failed request is never replayed"
    );
    harness.finish().await;
}

/// Once abnormal restarts fill the supervisor's window, it refuses the next
/// one, and the call says how long the worker stays down rather than opening
/// a new supervisor with an empty window.
#[tokio::test]
async fn a_full_restart_window_keeps_the_worker_down_and_says_for_how_long() {
    let harness = ActorHarness::start(true);
    // A timeout, then a failed start for each call until the window is full.
    for _ in 0..3 {
        let _ = query(&harness.project).await.expect_err("no child can start");
    }
    let (result, evidence) =
        crate::cancellation::with_cancellation_evidence(&CancellationToken::new(), query(&harness.project)).await;
    let refused = failure(result.expect_err("the supervisor refuses a fourth abnormal restart"));
    assert!(
        refused.reason.starts_with("restart_limit_exceeded"),
        "{}",
        refused.reason
    );
    assert!(refused.retryable, "the worker returns once the window clears");
    let wait = refused.retry_after_millis.expect("the refusal says how long to wait");
    assert!(wait > 0 && wait <= 60_000, "{wait}");
    assert_eq!(evidence.not_started, 1, "the refused call reached no child");
    assert!(!evidence.uncertain);
    assert!(harness.project.is_healthy(), "the actor waits out the window");
    assert!(harness.project.restart_retry_after().is_some());
    assert!(
        harness
            .broker
            .worker_restart_retry_after(&harness.project.canonical_root)
            .is_some(),
        "lean_status can report the wait"
    );
    assert_eq!(harness.requests("process_module_query").len(), 1);
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
                admission_deadline(),
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
    // The supervisor replaced the child, so the actor stays warm.
    assert!(harness.project.is_healthy(), "a replaced child keeps its actor");
    assert_eq!(harness.requests("spawn").len(), 2, "one replacement generation");
    assert_eq!(harness.tracker.active_processes(), 1, "the replacement owns capacity");
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
        // A call sets this marker when earlier owned work, such as a semantic
        // runtime build, has an unresolved outcome. The main call below still
        // runs a real child.
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

/// A completed call leaves its worker's receipt, and the broker reads it for
/// `lean_status` without sending the worker anything.
#[tokio::test]
async fn a_completed_call_leaves_a_receipt_for_status() {
    let harness = ActorHarness::start(false);
    let root = harness.project.canonical_root.clone();
    assert!(
        harness
            .broker
            .worker_compatibility(&root)
            .expect("compiled contract")
            .is_none(),
        "no call has completed"
    );
    assert!(harness.requests("open_host_session").is_empty());
    open_session(&harness.project, admission_deadline())
        .await
        .expect("actual fake metadata compatibility");
    let evidence = harness
        .broker
        .worker_compatibility(&root)
        .expect("compiled contract")
        .expect("the completed call left a receipt");
    assert_eq!(evidence.server_instance_id, harness.broker.instance_id());
    assert_eq!(evidence.loaded.len(), 1);
    assert_eq!(evidence.loaded[0].worker_generation, 1);
    assert_eq!(evidence.loaded[0].worker_session_id, harness.project.session_id);
    assert_eq!(harness.requests("open_host_session").len(), 1);
    assert_eq!(harness.requests("host_runtime_metadata").len(), 1);
    assert!(harness.requests("process_module_query").is_empty());
    assert!(harness.requests("attempt_proof").is_empty());
    assert_eq!(harness.tracker.active_processes(), 1);
    assert_eq!(harness.capacity.available_permits(), 0);
    harness.finish().await;
}

/// A receipt names one worker generation. A replacement that has completed
/// no call has no receipt to report.
#[tokio::test]
async fn a_replaced_worker_drops_the_old_receipt() {
    let harness = ActorHarness::start(false);
    let root = harness.project.canonical_root.clone();
    open_session(&harness.project, admission_deadline())
        .await
        .expect("a completed call on the first generation");
    assert!(
        harness
            .broker
            .worker_compatibility(&root)
            .expect("compiled contract")
            .is_some()
    );
    assert!(
        query(&harness.project).await.is_err(),
        "the fake worker never answers this query"
    );
    assert_eq!(harness.requests("spawn").len(), 2, "one replacement generation");
    assert!(
        harness
            .broker
            .worker_compatibility(&root)
            .expect("compiled contract")
            .is_none(),
        "the replacement has completed no call"
    );
    harness.finish().await;
}

#[tokio::test]
async fn incompatible_session_metadata_refuses_before_target_body() {
    let harness = ActorHarness::start(false);
    open_session(&harness.project, admission_deadline())
        .await
        .expect("establish actual contract");
    let mut incompatible =
        crate::runtime_compatibility::session_expectation("leanprover/lean4:v4.35.0", None).expect("known toolchain");
    incompatible
        .checked_host_bindings
        .insert("verify_declaration_batch".into(), "b".repeat(64));
    // Fault-inject the fake child's next loaded metadata frame. The same
    // session-open validator runs after a real internal recycle as well.
    write_message(
        harness.root.path(),
        "metadata",
        Message::Response(Response::HostRuntimeMetadata {
            metadata: lean_rs_worker_parent::LeanWorkerHostRuntimeMetadata {
                compiled_lean_version: "4.35.0".into(),
                compiled_header_sha256: incompatible.compiled_header_sha256.expect("header"),
                runtime_lean_version: Some("4.35.0".into()),
                checked_host_bindings: incompatible.checked_host_bindings,
            },
        }),
    );
    let result = query(&harness.project).await;
    assert!(result.is_err());
    assert!(harness.requests("process_module_query").is_empty());
    assert!(harness.requests("attempt_proof").is_empty());
    harness.finish().await;
}

/// A child that dies while idle stays with its supervisor. The next call
/// replaces it once and runs on the replacement; the replacement counts
/// against the restart window, which a new actor would have started empty.
#[tokio::test]
async fn idle_worker_death_is_replaced_by_the_next_call() {
    let harness = ActorHarness::start_with_runtime(false, Some(tokio::runtime::Handle::current()), None, None);
    assert!(
        harness.project.is_healthy(),
        "initial owner status observed a live worker"
    );
    tokio::time::sleep(WORKER_LIVENESS_POLL_INTERVAL + Duration::from_millis(50)).await;
    assert!(
        harness.project.is_healthy(),
        "an ordinary healthy idle poll keeps the worker warm"
    );
    fs::write(harness.root.path().join("idle_exit"), "").expect("release worker after healthy observation");
    tokio::time::timeout(TEST_DEADLINE, async {
        while !harness.project.runtime.lock().child_exited {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("an idle poll observes the exit");
    assert!(harness.root.path().join("idle_exited").is_file());
    assert!(harness.project.is_healthy(), "the supervisor keeps the actor");
    assert_eq!(
        harness.project.live_generation(),
        None,
        "no cached answer outlives the child that computed it"
    );
    fs::remove_file(harness.root.path().join("idle_exit")).expect("let the replacement live");

    let call = open_session(&harness.project, admission_deadline())
        .await
        .expect("the replacement serves the call");
    let restart = call
        .runtime
        .call_restart
        .as_ref()
        .expect("the call paid for the replacement");
    assert_eq!(restart.cause, "child_abort", "the child exited with status 23");
    assert!(!restart.planned);
    assert_eq!(call.runtime.worker_generation, 2);
    assert_eq!(call.runtime.final_attempt, FinalAttemptStatus::Completed);
    assert!(
        crate::diagnosis::execution_taint(&call.runtime).is_none(),
        "the call ran once, on the replacement"
    );
    assert_eq!(harness.project.live_generation(), Some(2));
    assert!(harness.requests("process_module_query").is_empty());
    assert_eq!(harness.requests("spawn").len(), 2, "one replacement");
    harness.finish().await;
}

#[tokio::test]
async fn lifecycle_polls_keep_healthy_idle_workers_warm_without_health_rpcs() {
    let harness = ActorHarness::start_with_runtime(false, Some(tokio::runtime::Handle::current()), None, None);
    tokio::time::sleep(WORKER_LIVENESS_POLL_INTERVAL * 3).await;
    assert!(harness.project.is_healthy());
    assert_eq!(harness.requests("spawn").len(), 1);
    assert_eq!(harness.requests("health").len(), 1, "bootstrap is the only health RPC");
    assert_eq!(harness.tracker.active_processes(), 1);
    assert_eq!(harness.capacity.available_permits(), 0);
    harness.finish().await;
}

#[tokio::test]
async fn lifecycle_ticks_preserve_residue_quiescence_and_one_cycle_per_quiet_period() {
    let harness = ActorHarness::start_with_runtime(false, Some(tokio::runtime::Handle::current()), Some(1), None);
    let mut bytes = fs::File::open(harness.root.path().join("session")).expect("import response");
    let frame =
        lean_rs_worker_protocol::protocol::read_frame(&mut bytes, MAX_FRAME_BYTES).expect("read import response");
    let Message::Response(Response::HostSessionOpened { mut import_stats }) = frame.message else {
        panic!("host import response")
    };
    import_stats.non_memory_mapped_region_bytes = 1;
    write_message(
        harness.root.path(),
        "session",
        Message::Response(Response::HostSessionOpened { import_stats }),
    );
    let request_started = Instant::now();
    let call = harness
        .project
        .search_declarations(vec!["Init".into()], Vec::new(), admission_deadline())
        .await
        .expect("one import and no searches");
    assert_eq!(
        call.runtime.import_residue_bytes,
        Some(1),
        "fixture import was accounted"
    );
    tokio::time::sleep(QUIESCENCE_GRACE.saturating_sub(WORKER_LIVENESS_POLL_INTERVAL)).await;
    assert_eq!(
        harness.requests("spawn").len(),
        1,
        "health ticks do not trigger a premature residue cycle"
    );
    tokio::time::timeout(TEST_DEADLINE, async {
        while harness.requests("spawn").len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "health ticks do not reset the quiet-period clock: {error}; healthy={} requests={:?} runtime={:?}",
            harness.project.is_healthy(),
            fs::read_to_string(harness.root.path().join("requests")),
            harness.project.runtime_facts(),
        )
    });
    assert!(request_started.elapsed() >= QUIESCENCE_GRACE);
    tokio::time::sleep(QUIESCENCE_GRACE + WORKER_LIVENESS_POLL_INTERVAL).await;
    assert_eq!(
        harness.requests("spawn").len(),
        2,
        "prewarming cannot spend the same quiet period twice"
    );
    harness.finish().await;
}

/// Run one call the way a request does: under a read turn on the project.
async fn call_under_turn(
    harness: &ActorHarness,
    lock: &crate::project_lock::ProjectLock,
) -> (u64, ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>) {
    crate::project_lock::scope(async {
        let granted = crate::project_lock::ensure_read(
            lock,
            &harness.project.canonical_root,
            admission_deadline().into(),
            &CallCancellation::default(),
        )
        .await
        .expect("read turn on the fixture");
        let call = Box::pin(open_session(&harness.project, admission_deadline()))
            .await
            .expect("the fake worker serves the call");
        (granted, call)
    })
    .await
}

/// A build stops the project's workers before Lake runs; this fake worker
/// lives through a finished write turn instead. The count then makes the
/// actor replace it once, before the next job.
#[tokio::test]
async fn a_build_finished_since_load_replaces_the_worker_once() {
    use crate::project_lock::{ProjectLock, ensure_write};

    let harness = ActorHarness::start_with_runtime(false, None, None, Some(TEST_DEADLINE));
    let lock = Arc::new(ProjectLock::default());
    let (granted, first) = call_under_turn(&harness, &lock).await;
    assert_eq!(granted, 0);
    assert!(first.runtime.call_restart.is_none(), "{:?}", first.runtime.call_restart);
    let until = tokio::time::Instant::now() + TEST_DEADLINE;
    ensure_write(&lock, "build", until, &crate::cancellation::CallCancellation::default())
        .await
        .expect("a free write turn")
        .finish();
    assert_eq!(lock.builds_finished(), 1);
    let (granted, second) = call_under_turn(&harness, &lock).await;
    assert_eq!(granted, 1);
    let restart = second
        .runtime
        .call_restart
        .as_ref()
        .expect("a new worker serves the call");
    assert_eq!(restart.cause, "artifacts_rebuilt");
    assert_eq!(second.runtime.worker_generation, 2);
    let (_, third) = call_under_turn(&harness, &lock).await;
    assert!(
        third.runtime.call_restart.is_none(),
        "the new worker loaded after the build: {:?}",
        third.runtime.call_restart
    );
    assert_eq!(harness.requests("spawn").len(), 2, "one replacement");
    harness.finish().await;
}

/// A missing import, after a stopped older request changed the worker
/// generation, refuses before the target body and reuses no old stop receipt.
/// No Lean or Lake runs.
#[tokio::test]
async fn missing_import_after_replacement_refuses_before_the_target_body() {
    let harness = ActorHarness::start(false);
    let prior_request = CancellationToken::new();
    let prior = crate::cancellation::with_cancellation(&prior_request, query(&harness.project)).await;
    let prior = failure(prior.expect_err("first fixture request times out and is reaped"));
    assert!(prior.worker_stop_receipt.is_some(), "older exact stop evidence exists");
    fs::write(harness.root.path().join("require_missing_import"), "").expect("request absent dependency");
    let artifacts = harness.root.path().join(".lake/build/lib/lean");
    fs::create_dir_all(&artifacts).expect("fixture artifact directory");
    fs::write(artifacts.join("A.olean"), "named-import-present").expect("fixture named artifact stamp");
    assert!(!artifacts.join("B.olean").exists());
    let result = harness
        .project
        .process_module_query(
            vec!["A".to_owned()],
            "theorem UnitIndexed : True := True.intro".into(),
            LeanWorkerModuleQuery::Diagnostics,
            LeanWorkerElabOptions::new().file_label("UnitIndexed.lean"),
            admission_deadline(),
        )
        .await;
    let error = result.as_ref().expect_err("missing import refuses before target body");
    assert!(crate::diagnosis::missing_olean_failure(error));
    let ServerError::WorkerUnavailable(error) = error else {
        panic!("worker metadata error")
    };
    assert_eq!(error.runtime.worker_generation, 2);
    assert_eq!(error.runtime.retry_count, 0);
    assert_eq!(error.runtime.final_attempt, FinalAttemptStatus::Completed);
    assert!(
        error.worker_stop_receipt.is_none(),
        "older receipt cannot settle a new request"
    );
    assert_eq!(
        harness.requests("process_module_query").len(),
        1,
        "new target body never ran"
    );
    assert_eq!(harness.requests("spawn").len(), 2, "no replay or third generation");
    assert!(
        harness.project.is_healthy(),
        "a returned import error keeps the ready worker usable"
    );
    assert_eq!(
        harness.tracker.active_processes(),
        1,
        "only the healthy replacement is resident"
    );
    harness.finish().await;
}

#[tokio::test]
async fn startup_owner_keeps_returned_evidence_when_initializer_reply_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    let project = fixture_project(root.path());
    let tracker = LeanWorkerProcessTracker::new();
    let config = fixture_config(root.path(), fake_worker(root.path(), false), &project, &tracker);
    let (reply, receive) = std::sync::mpsc::channel();
    drop(receive);
    let request = CancellationToken::new();
    let ((), evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
        let operation = CallCancellation::current().begin_operation();
        tokio::time::timeout(
            TEST_DEADLINE,
            tokio::task::spawn_blocking(move || actor_main(config, reply, operation)),
        )
        .await
        .expect("bounded startup with vanished reply")
        .expect("startup owner joined");
    })
    .await;
    assert_eq!(tracker.active_processes(), 0);
    assert_eq!(requests_at(root.path(), "spawn").len(), 1);
    assert_eq!(evidence.pending, 0);
    assert_eq!(evidence.returned, 1);
    assert!(!evidence.uncertain);
    assert!(evidence.deferred.is_empty());
    assert!(requests_at(root.path(), "attempt_proof").is_empty());
}

#[tokio::test]
async fn startup_failure_after_confirmed_child_cleanup_is_returned_even_without_reply_waiter() {
    for drop_reply in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let project = fixture_project(root.path());
        let tracker = LeanWorkerProcessTracker::new();
        let executable = fake_worker(root.path(), false);
        write_message(
            root.path(),
            "healthy",
            Message::Response(Response::Error {
                code: "fixture.startup_failed".into(),
                message: "health failed before any proof".into(),
            }),
        );
        let config = fixture_config(root.path(), executable, &project, &tracker);
        let uncertain = Arc::clone(&config.startup_uncertain);
        let (reply, receive) = std::sync::mpsc::channel();
        let receive = if drop_reply {
            drop(receive);
            None
        } else {
            Some(receive)
        };
        let request = CancellationToken::new();
        let ((), evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            let operation = CallCancellation::current().begin_operation();
            tokio::time::timeout(
                TEST_DEADLINE,
                tokio::task::spawn_blocking(move || actor_main(config, reply, operation)),
            )
            .await
            .expect("bounded failed startup")
            .expect("startup owner joined");
        })
        .await;
        if let Some(receive) = receive {
            assert!(receive.recv_timeout(TEST_DEADLINE).unwrap().is_err());
        }
        assert_eq!(requests_at(root.path(), "spawn").len(), 1);
        assert_eq!(tracker.active_processes(), 0);
        assert!(!uncertain.load(Ordering::Acquire));
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.returned, 1);
        assert!(!evidence.uncertain);
        assert!(evidence.deferred.is_empty());
        assert!(requests_at(root.path(), "attempt_proof").is_empty());
    }
}

#[tokio::test]
async fn failed_startup_retains_exact_tracked_scope_until_owned_child_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let tracker = LeanWorkerProcessTracker::new();
    let worker = tracked_worker(root.path(), &tracker);
    let project = fixture_project(root.path());
    let config = fixture_config(root.path(), root.path().join("missing-worker"), &project, &tracker);
    let uncertain = Arc::clone(&config.startup_uncertain);
    let (reply, receive) = std::sync::mpsc::channel();
    let request = CancellationToken::new();
    let ((), mut evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
        let operation = CallCancellation::current().begin_operation();
        tokio::task::spawn_blocking(move || actor_main(config, reply, operation))
            .await
            .unwrap();
    })
    .await;
    assert!(receive.recv_timeout(TEST_DEADLINE).unwrap().is_err());
    assert!(uncertain.load(Ordering::Acquire));
    assert_eq!(tracker.active_processes(), 1);
    assert_eq!(evidence.pending, 0);
    assert_eq!(evidence.returned, 0);
    assert_eq!(evidence.deferred.len(), 1);
    evidence.refresh_deferred();
    assert_eq!(evidence.deferred.len(), 1);
    worker.shutdown().expect("stop this exact owned tracked child");
    assert_eq!(tracker.active_processes(), 0);
    evidence.refresh_deferred();
    assert!(evidence.deferred.is_empty());
    assert!(evidence.uncertain);
    assert_eq!(evidence.returned, 0);
    assert_eq!(evidence.stopped, 0);
    assert!(
        evidence.worker_stop_receipt.is_none(),
        "physical closure does not invent request status"
    );
}

fn alter_runtime_metadata(root: &Path, change: impl FnOnce(&mut lean_rs_worker_parent::LeanWorkerHostRuntimeMetadata)) {
    let bytes = fs::read(root.join("metadata")).expect("fake loaded metadata");
    let frame = lean_rs_worker_protocol::protocol::read_frame(&mut std::io::Cursor::new(bytes), MAX_FRAME_BYTES)
        .expect("decode fixture metadata");
    let Message::Response(Response::HostRuntimeMetadata { mut metadata }) = frame.message else {
        panic!("fixture must carry loaded metadata");
    };
    change(&mut metadata);
    write_message(
        root,
        "metadata",
        Message::Response(Response::HostRuntimeMetadata { metadata }),
    );
}

#[tokio::test]
async fn cold_compiled_abi_header_and_runtime_refuse_before_first_target_body() {
    for mismatch in ["binding", "missing", "header", "runtime"] {
        let harness = ActorHarness::start(false);
        assert!(harness.requests("open_host_session").is_empty());
        alter_runtime_metadata(harness.root.path(), |actual| match mismatch {
            "binding" => {
                actual
                    .checked_host_bindings
                    .insert("verify_declaration_batch".into(), "wrong-abi".into());
            }
            "missing" => {
                actual.checked_host_bindings.remove("process_module_query_batch_cached");
            }
            "header" => {
                actual.compiled_header_sha256 = "wrong-header".into();
            }
            "runtime" => {
                actual.runtime_lean_version = Some("4.34.0".into());
            }
            _ => panic!("unknown test mismatch"),
        });
        let (result, evidence) = with_cancellation_evidence(&CancellationToken::new(), async {
            harness
                .project
                .process_module_query(
                    Vec::new(),
                    "theorem fixture : True := True.intro".into(),
                    LeanWorkerModuleQuery::Diagnostics,
                    LeanWorkerElabOptions::default().file_label("Cold.lean"),
                    admission_deadline(),
                )
                .await
        })
        .await;
        let failure = failure(result.expect_err("cold incompatibility must refuse"));
        assert_eq!(
            failure.runtime.final_attempt,
            FinalAttemptStatus::Completed,
            "{mismatch}"
        );
        assert!(
            failure.worker_stop_receipt.is_none(),
            "metadata refusal is not an exit witness"
        );
        assert_eq!(evidence.returned, 1, "{mismatch}");
        assert_eq!(evidence.stopped, 0, "{mismatch}");
        assert!(!evidence.uncertain, "{mismatch}");
        let target = evidence.target_check.expect("known target phase");
        assert_eq!(target.file_label, "Cold.lean");
        assert_eq!(target.stage, "host_runtime_validation");
        assert_eq!(target.disposition, "not_started");
        assert_eq!(harness.requests("host_runtime_metadata").len(), 1);
        assert!(harness.requests("process_module_query").is_empty());
        assert!(harness.requests("attempt_proof").is_empty());
        harness.wait_for_retirement().await;
        harness.finish().await;
    }
}

#[test]
fn compiled_abi_guard_survives_an_actual_worker_replacement() {
    let root = tempfile::tempdir().expect("private replacement fixture");
    let tracker = LeanWorkerProcessTracker::new();
    let (_project, mut state) = ordinary_state(root.path(), &tracker);
    {
        let _session = state
            .handle
            .open_session(None, None)
            .expect("valid first loaded session");
    }
    assert_eq!(requests_at(root.path(), "spawn").len(), 1);
    alter_runtime_metadata(root.path(), |actual| {
        actual
            .checked_host_bindings
            .insert("verify_declaration_batch".into(), "wrong-replacement-abi".into());
    });
    state.handle.cycle().expect("actual worker replacement");
    assert_eq!(requests_at(root.path(), "spawn").len(), 2);
    let result = state.handle.process_module_query_with_imports(
        Vec::new(),
        "theorem fixture : True := True.intro",
        &LeanWorkerModuleQuery::Diagnostics,
        &LeanWorkerElabOptions::default(),
        None,
        None,
    );
    assert!(result.is_err());
    assert_eq!(requests_at(root.path(), "host_runtime_metadata").len(), 2);
    assert!(requests_at(root.path(), "process_module_query").is_empty());
    state.handle.try_shutdown().expect("refused replacement reaped");
    assert_eq!(tracker.active_processes(), 0);
}

#[tokio::test]
async fn unexpected_metadata_transport_response_does_not_claim_known_refusal() {
    let harness = ActorHarness::start(false);
    write_message(harness.root.path(), "metadata", Message::Response(Response::HealthOk));
    let (result, evidence) = with_cancellation_evidence(&CancellationToken::new(), async {
        harness
            .project
            .process_module_query(
                Vec::new(),
                "theorem fixture : True := True.intro".into(),
                LeanWorkerModuleQuery::Diagnostics,
                LeanWorkerElabOptions::default().file_label("Unknown.lean"),
                admission_deadline(),
            )
            .await
    })
    .await;
    let failure = failure(result.expect_err("wrong typed response must refuse"));
    assert_eq!(failure.runtime.final_attempt, FinalAttemptStatus::Interrupted);
    assert_eq!(evidence.returned, 0);
    assert!(
        evidence.uncertain || !evidence.deferred.is_empty() || evidence.worker_stop_receipt.is_some(),
        "transport failure needs uncertainty or its own genuine stop observer: {evidence:?}",
    );
    assert!(evidence.target_check.is_none());
    assert!(harness.requests("process_module_query").is_empty());
    harness.finish().await;
}
