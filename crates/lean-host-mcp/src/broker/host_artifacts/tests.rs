//! A fake worker script stands in for the readiness mode; no Lean runtime or
//! build runs.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "private process fixtures assert readiness, cancellation and evidence"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

const LIMIT: Duration = Duration::from_secs(3);
/// A worker that logs each flag it gets. In readiness mode it exits 75 unless
/// `ready` exists, and waits for `go` while `hold` exists.
const WORKER: &str = r"#!/usr/bin/env python3
import os, signal, sys, time
from pathlib import Path
root = Path(__file__).parent
signal.alarm(5)
with (root / 'events').open('a') as events:
    events.write(sys.argv[1] + '\n')
if sys.argv[1] != '--lean-rs-ready-host-artifacts':
    sys.exit(19)
if (root / 'hold').exists():
    (root / 'probing.tmp').write_text(str(os.getpid()))
    (root / 'probing.tmp').replace(root / 'probing')
    while not (root / 'go').exists():
        time.sleep(0.01)
if not (root / 'ready').exists():
    sys.exit(75)
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

    async fn read(&self, cancellation: &CallCancellation) -> Result<LeanWorkerPreparedHostArtifacts> {
        installed_host_artifacts(
            &self.root,
            &self.worker,
            &self.sysroot,
            &self.writer,
            tokio::time::Instant::now().checked_add(LIMIT).unwrap(),
            cancellation,
        )
        .await
    }

    /// The flags the worker got, in order.
    fn events(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn broker() -> Arc<ProjectBroker> {
    ProjectBroker::new(super::super::BrokerConfig {
        lake_root: None,
        max_workers: std::num::NonZeroUsize::MIN,
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
async fn installed_shims_answer_with_their_descriptor() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("ready"), "installed").unwrap();
    assert_eq!(
        fixture.read(&CallCancellation::default()).await.unwrap(),
        fixture.artifacts
    );
    assert_eq!(fixture.events(), ["--lean-rs-ready-host-artifacts"]);
}

#[tokio::test]
async fn missing_shims_fail_with_the_install_command_and_build_nothing() {
    let fixture = Fixture::new();
    let error = fixture.read(&CallCancellation::default()).await.unwrap_err();
    assert!(
        matches!(&error, ServerError::BadProject(message) if message.contains("install --lake-root")),
        "{error:?}"
    );
    assert_eq!(fixture.events(), ["--lean-rs-ready-host-artifacts"]);
}

#[tokio::test]
async fn a_dropped_waiter_stops_the_readiness_child() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("hold"), "hold").unwrap();
    let cancellation = CallCancellation::default();
    let mut read = Box::pin(fixture.read(&cancellation));
    tokio::select! {
        result = &mut read => panic!("readiness returned before the waiter dropped: {result:?}"),
        () = wait_for(|| fixture.root.join("probing").exists()) => {},
    }
    let pid: i32 = fs::read_to_string(fixture.root.join("probing"))
        .unwrap()
        .parse()
        .unwrap();
    drop(read);
    assert!(cancellation.is_cancelled());
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    wait_for(|| rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)).await;
    assert_eq!(fixture.events(), ["--lean-rs-ready-host-artifacts"]);
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
        fs::write(fixture.root.join("ready"), "installed").unwrap();
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
                let probe = installed_host_artifacts(
                    &fixture.root,
                    &fixture.worker,
                    &fixture.sysroot,
                    &fixture.writer,
                    deadline,
                    &cancellation,
                );
                tokio::pin!(probe);
                // The only pending await is the already-submitted blocking
                // task. An entry-time refusal is Ready.
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
            assert!(fixture.events().is_empty(), "no readiness command reached the fixture");
        }
    });
}
