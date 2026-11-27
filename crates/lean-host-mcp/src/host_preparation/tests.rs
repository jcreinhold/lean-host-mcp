//! Test-owned executable fixtures; no Lean runtime, Lake build or service.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    reason = "private process fixtures assert actual preparation ownership and bounded failure"
)]

use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

const LIMIT: Duration = Duration::from_secs(2);
const FAKE_WORKER: &str = r"#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
root = Path(__file__).parent
signal.alarm(4)
(root / 'pid').write_text(str(os.getpid()))
(root / 'environment').write_text(json.dumps(dict(os.environ)))
(root / 'arguments').write_text(json.dumps(sys.argv[1:]))
mode = (root / 'mode').read_text()
if mode == 'timeout':
    while True:
        time.sleep(0.01)
elif mode == 'stdout_limit':
    while True:
        os.write(1, b'x' * 4096)
elif mode == 'stderr_limit':
    while True:
        os.write(2, b'x' * 4096)
elif mode == 'not_ready':
    sys.exit(75)
elif mode == 'probe_only':
    if sys.argv[1:] != ['--lean-rs-ready-host-artifacts']:
        (root / 'writer-started').write_text('unexpected preparation')
        sys.exit(17)
    os.write(1, (root / 'response').read_bytes())
elif mode == 'double_json':
    os.write(1, (root / 'response').read_bytes())
    os.write(1, b'{}')
elif mode == 'failed':
    os.write(2, b'independent writer closure is unknown')
    sys.exit(17)
else:
    os.write(1, (root / 'response').read_bytes() + b'\n')
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
    fn new(mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let worker = root.join("fake-preparation-worker");
        fs::write(&worker, FAKE_WORKER).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        let sysroot = root.join("sysroot");
        let search = root.join("search");
        fs::create_dir(&sysroot).unwrap();
        fs::create_dir(&search).unwrap();
        let writer = root.join("writer");
        fs::write(&writer, "private writer identity").unwrap();
        let manifest = root.join("shim.json");
        let manifest_json = "{\"storage_fixture\":true}";
        fs::write(&manifest, manifest_json).unwrap();
        let artifacts = LeanWorkerPreparedHostArtifacts {
            protocol: 1,
            shim_manifest_path: manifest.to_string_lossy().into_owned(),
            shim_manifest_json: manifest_json.to_owned(),
            artifact_roots: vec![sysroot.to_string_lossy().into_owned()],
            shim_search_paths: vec![search.to_string_lossy().into_owned()],
        };
        fs::write(root.join("response"), serde_json::to_vec(&artifacts).unwrap()).unwrap();
        fs::write(root.join("mode"), mode).unwrap();
        Self {
            _directory: directory,
            root,
            worker,
            sysroot,
            writer,
            artifacts,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.worker);
        configure(
            &mut command,
            &self.sysroot,
            &self.writer,
            &self.root,
            &[OsStr::new(PREPARE_FLAG)],
        )
        .unwrap();
        command
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < LIMIT,
            "pre-created owner did not reap its test child"
        );
        thread::sleep(POLL);
    }
}

#[test]
fn success_requires_real_child_exit_and_exact_disk_snapshot() {
    let fixture = Fixture::new("success");
    let actual = prepare(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT).unwrap();
    assert_eq!(actual, fixture.artifacts);
    let args: Vec<String> = serde_json::from_slice(&fs::read(fixture.root.join("arguments")).unwrap()).unwrap();
    assert_eq!(args, ["--lean-rs-prepare-host-artifacts"]);
}

#[test]
fn actual_preparation_command_scrubs_inherited_native_ownership() {
    use lean_toolchain::shared_writer::SHARED_WRITER_EXECUTABLE_ENV;

    let fixture = Fixture::new("success");
    let mut command = Command::new(&fixture.worker);
    for key in SCRUB_ENV.iter().chain(WRITER_HANDOFF_ENV.iter()) {
        command.env(key, "must-not-reach-preparation");
    }
    command
        .env(OWNERS_DIRECTORY_ENV, "ignored-registry")
        .env("LEAN_SYSROOT", "wrong");
    configure(
        &mut command,
        &fixture.sysroot,
        &fixture.writer,
        &fixture.root,
        &[OsStr::new(PREPARE_FLAG)],
    )
    .unwrap();
    start(command, LIMIT).unwrap().wait().unwrap();
    let environment: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.root.join("environment")).unwrap()).unwrap();
    for key in SCRUB_ENV.iter().chain(WRITER_HANDOFF_ENV.iter()) {
        assert!(environment.get(*key).is_none(), "inherited {key} must be removed");
    }
    assert_eq!(environment[OWNERS_DIRECTORY_ENV].as_str(), fixture.root.to_str());
    assert_eq!(environment["LEAN_SYSROOT"].as_str(), fixture.sysroot.to_str());
    assert_eq!(
        environment[SHARED_WRITER_EXECUTABLE_ENV].as_str(),
        fixture.writer.to_str()
    );
}

#[test]
fn timeout_reaps_actual_preparation_child_but_does_not_certify_writers() {
    let fixture = Fixture::new("timeout");
    let started = Instant::now();
    let error = prepare(
        &fixture.worker,
        &fixture.sysroot,
        &fixture.writer,
        Duration::from_millis(80),
    )
    .unwrap_err();
    assert!(started.elapsed() < LIMIT);
    assert_eq!(error.disposition, WriterDisposition::Unknown);
    assert!(error.preparation_child_stopped, "actual owned child was waited on");
}

#[test]
fn output_limits_apply_to_both_pipes_and_do_not_authorize_a_result() {
    for mode in ["stdout_limit", "stderr_limit"] {
        let fixture = Fixture::new(mode);
        let error = prepare(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT).unwrap_err();
        assert!(error.message.contains("exceeded"), "{error}");
        assert_eq!(error.disposition, WriterDisposition::Unknown);
        assert!(error.preparation_child_stopped);
    }
}

#[test]
fn abandoned_wait_signals_the_existing_owner_and_keeps_actual_reaping() {
    let fixture = Fixture::new("timeout");
    // Neither the 30s request deadline nor the fixture's 4s fallback can
    // explain a stop observed within the 2s cancellation bound below.
    let pending = start(fixture.command(), Duration::from_secs(30)).unwrap();
    let observed = Arc::clone(&pending.control);
    wait_until(|| fixture.root.join("pid").is_file());
    drop(pending);
    wait_until(|| observed.child_stopped.load(Ordering::Acquire));
}

#[test]
fn no_spawn_is_distinct_from_failed_or_invalid_post_spawn_output() {
    let fixture = Fixture::new("failed");
    let error = prepare(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT).unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::Unknown);
    assert!(error.preparation_child_stopped);
    assert!(error.message.contains("independent writer closure is unknown"));
    let error = prepare(
        &fixture.root.join("missing-worker"),
        &fixture.sysroot,
        &fixture.writer,
        LIMIT,
    )
    .unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::NotStarted);
    assert!(!error.preparation_child_stopped);
    let fixture = Fixture::new("double_json");
    let error = prepare(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT).unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::Unknown);
    assert!(error.preparation_child_stopped);
}

#[test]
fn strict_storage_validation_rejects_wrong_protocol_paths_shape_and_snapshot() {
    let fixture = Fixture::new("success");
    let encode = |value: &LeanWorkerPreparedHostArtifacts| serde_json::to_vec(value).unwrap();
    let mut invalid = fixture.artifacts.clone();
    invalid.protocol = 2;
    assert!(validate(&encode(&invalid)).is_err());
    invalid = fixture.artifacts.clone();
    invalid.shim_manifest_json = "{}".to_owned();
    assert!(validate(&encode(&invalid)).is_err());
    invalid = fixture.artifacts.clone();
    invalid.shim_manifest_path = fixture.root.join(".").join("shim.json").to_string_lossy().into_owned();
    assert!(validate(&encode(&invalid)).is_err());
    invalid = fixture.artifacts.clone();
    invalid.artifact_roots = vec![fixture.sysroot.to_string_lossy().into_owned(); 33];
    assert!(validate(&encode(&invalid)).is_err());
    invalid = fixture.artifacts.clone();
    invalid.shim_search_paths = vec![fixture.root.join("missing").to_string_lossy().into_owned()];
    assert!(validate(&encode(&invalid)).is_err());
    let mut extra = serde_json::to_value(&fixture.artifacts).unwrap();
    extra["untrusted_extra"] = serde_json::json!(true);
    assert!(validate(&serde_json::to_vec(&extra).unwrap()).is_err());
}

#[test]
fn readiness_probe_uses_only_read_mode_and_never_falls_through_to_a_writer() {
    let fixture = Fixture::new("probe_only");
    let actual = probe_ready(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT, &|| false).unwrap();
    assert_eq!(actual, Some(fixture.artifacts.clone()));
    assert!(!fixture.root.join("writer-started").exists());
    let args: Vec<String> = serde_json::from_slice(&fs::read(fixture.root.join("arguments")).unwrap()).unwrap();
    assert_eq!(args, ["--lean-rs-ready-host-artifacts"]);

    fs::write(fixture.root.join("mode"), "not_ready").unwrap();
    assert!(
        probe_ready(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT, &|| false)
            .unwrap()
            .is_none()
    );
    let error = prepare(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT).unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::Unknown);
    assert!(error.preparation_child_stopped);
}

#[test]
fn caller_cancellation_before_launch_does_not_spawn() {
    let fixture = Fixture::new("timeout");
    let error = run_artifact_mode(
        &fixture.worker,
        &fixture.sysroot,
        &fixture.writer,
        &[OsStr::new(PREPARE_FLAG)],
        LIMIT,
        &|| true,
    )
    .unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::NotStarted);
    assert!(!fixture.root.join("pid").exists());
    let error = probe_ready(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT, &|| true).unwrap_err();
    assert_eq!(error.disposition, WriterDisposition::NotStarted);
    assert!(!fixture.root.join("pid").exists());
}

#[test]
fn caller_cancellation_reaps_both_modes_before_their_long_deadline() {
    for ready in [false, true] {
        let fixture = Fixture::new("timeout");
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = fixture.worker.clone();
        let sysroot = fixture.sysroot.clone();
        let writer = fixture.writer.clone();
        let token = Arc::clone(&cancelled);
        let wait = thread::spawn(move || {
            let cancelled = || token.load(Ordering::Acquire);
            if ready {
                probe_ready(&worker, &sysroot, &writer, Duration::from_secs(30), &cancelled)
            } else {
                run_artifact_mode(
                    &worker,
                    &sysroot,
                    &writer,
                    &[OsStr::new(PREPARE_FLAG)],
                    Duration::from_secs(30),
                    &cancelled,
                )
            }
        });
        wait_until(|| fixture.root.join("pid").is_file());
        let started = Instant::now();
        cancelled.store(true, Ordering::Release);
        let error = wait.join().unwrap().unwrap_err();
        assert!(started.elapsed() < LIMIT);
        assert_eq!(error.disposition, WriterDisposition::Unknown);
        assert!(error.preparation_child_stopped);
    }
}

#[tokio::test]
async fn abandoned_async_waiter_token_reaches_the_blocking_child_owner() {
    let fixture = Fixture::new("timeout");
    let pending = start(fixture.command(), Duration::from_secs(30)).unwrap();
    let observed = Arc::clone(&pending.control);
    let cancelled = Arc::new(AtomicBool::new(false));
    let token = Arc::clone(&cancelled);
    let waiter = tokio::task::spawn_blocking(move || pending.wait_cancellable(&|| token.load(Ordering::Acquire)));
    wait_until(|| fixture.root.join("pid").is_file());
    // Dropping this handle alone cannot stop a running spawn_blocking closure.
    drop(waiter);
    cancelled.store(true, Ordering::Release);
    wait_until(|| observed.child_stopped.load(Ordering::Acquire));
}

#[test]
fn readiness_probe_timeout_and_pipe_limits_do_not_report_ready() {
    for mode in ["stdout_limit", "stderr_limit"] {
        let fixture = Fixture::new(mode);
        let error = probe_ready(&fixture.worker, &fixture.sysroot, &fixture.writer, LIMIT, &|| false).unwrap_err();
        assert!(error.message.contains("exceeded"), "{error}");
        assert!(error.preparation_child_stopped);
    }
    let fixture = Fixture::new("timeout");
    let error = probe_ready(
        &fixture.worker,
        &fixture.sysroot,
        &fixture.writer,
        Duration::from_millis(80),
        &|| false,
    )
    .unwrap_err();
    assert!(error.preparation_child_stopped);
}
