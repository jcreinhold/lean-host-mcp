//! Exercise the real helper CLI through the public writer SDK and raw EOF.
#![cfg(target_os = "macos")]
#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    reason = "private bounded process fixtures assert exact lifecycle receipts"
)]

use std::cell::Cell;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lean_toolchain::process_group::group_present;
use lean_toolchain::shared_writer::{SharedWriter, WriterDisposition, WriterFailure, project_lock_path};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(7);
const CAPTURE_LIMIT: usize = 64 * 1024;
const HELPER: &str = env!("CARGO_BIN_EXE_lean-host-mcp");
const PAYLOAD: &str = r"
import os, pathlib, signal, sys, time
root, assets, mode = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.alarm(5)
if (assets / 'input').read_text() != 'owned input':
    sys.exit(122)
with (root / 'effects').open('a') as log:
    log.write('once\n')
if mode == 'clean':
    print('actual stdout', flush=True)
    print('actual stderr', file=sys.stderr, flush=True)
    sys.exit(0)
if mode == 'nul-flood':
    for index in range(256):
        for fd in [1, 2]:
            os.write(fd, b'\0' * 4096)
    sys.exit(0)
if mode == 'flood':
    for fd in [1, 2]:
        for index in range(256):
            os.write(fd, b'x' * 4096)
    sys.exit(0)
if mode == 'descendant':
    child = os.fork()
    if child != 0:
        until = time.monotonic() + 4
        while time.monotonic() < until and not (root / 'heartbeat').exists():
            time.sleep(0.002)
        (root / 'root-exit').write_text(str(os.getpid()))
        os._exit(0)
    signal.alarm(5)
    os.closerange(3, 4096)
    null = os.open('/dev/null', os.O_RDWR)
    for fd in [0, 1, 2]:
        os.dup2(null, fd)
    if null > 2:
        os.close(null)
with (root / 'heartbeat').open('ab', buffering=0) as output:
    until = time.monotonic() + 5
    while time.monotonic() < until:
        if not (assets / 'input').exists():
            (root / 'asset-lost').write_text('removed under live work')
        output.write(b'x')
        time.sleep(0.005)
os._exit(0)
";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    lock: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("project");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let lock = project_lock_path(&root).unwrap();
        Self {
            _directory: directory,
            root,
            lock,
        }
    }

    fn open(&self) -> SharedWriter {
        SharedWriter::open(Path::new(HELPER), &self.root, Duration::from_secs(3)).unwrap()
    }

    fn command(&self, assets: &Path, mode: &str) -> Command {
        let script = assets.join("payload.py");
        fs::write(&script, PAYLOAD).unwrap();
        fs::write(assets.join("input"), b"owned input").unwrap();
        let mut command = Command::new("python3");
        command
            .arg(script)
            .arg(&self.root)
            .arg(assets)
            .arg(mode)
            .current_dir(&self.root);
        command
    }

    fn effects(&self) -> usize {
        fs::read_to_string(self.root.join("effects")).map_or(0, |text| text.lines().count())
    }

    fn heartbeat(&self) -> u64 {
        fs::metadata(self.root.join("heartbeat")).map_or(0, |metadata| metadata.len())
    }

    fn assert_lock_held(&self) {
        let probe = OpenOptions::new().read(true).write(true).open(&self.lock).unwrap();
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
    }

    fn lock_available(&self) -> bool {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock)
            .is_ok_and(|file| file.try_lock().is_ok())
    }

    fn assert_stopped(&self, error: &io::Error) -> Value {
        let failure = WriterFailure::from_io(error).expect("public SDK must preserve typed execution evidence");
        assert_eq!(failure.disposition, WriterDisposition::Stopped, "{error}");
        let receipt = failure
            .stop_receipt
            .as_ref()
            .expect("stopped requires its terminal receipt");
        assert_eq!(receipt["state"], "stopped");
        assert_eq!(receipt["result"], "no_result");
        assert_eq!(receipt["owned_work_stopped"], true);
        assert_eq!(receipt["intervened"], true);
        assert!(receipt["exit_code"].is_i64());
        let pid = u32::try_from(receipt["process"]["pid"].as_u64().unwrap()).unwrap();
        assert_eq!(receipt["process"]["process_group"], pid);
        assert_ne!(
            receipt["owner"]["pid"], pid,
            "helper and actual command must retain distinct identities"
        );
        assert!(!group_present(pid).unwrap());
        let before = self.heartbeat();
        thread::sleep(Duration::from_millis(40));
        assert_eq!(self.heartbeat(), before, "payload writes survived the terminal receipt");
        assert!(!self.root.join("asset-lost").exists());
        assert_eq!(self.effects(), 1);
        receipt.clone()
    }

    fn assert_no_next_stage(&self, writer: &mut SharedWriter) {
        let marker = self.root.join("replayed");
        let mut next = Command::new("python3");
        next.args(["-c", "import pathlib, sys; pathlib.Path(sys.argv[1]).write_text('bad')"])
            .arg(&marker);
        assert!(writer.run(&next, Duration::from_secs(1)).is_err());
        assert!(!marker.exists());
        assert_eq!(self.effects(), 1);
    }

    fn release(&self, writer: SharedWriter, assets: &Path) {
        drop(writer);
        wait_until(|| !assets.exists() && self.lock_available());
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < LIMIT, "actual helper lifecycle did not settle");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn actual_helper_preserves_clean_output_lock_and_assets_until_sdk_drop() {
    let fixture = Fixture::new();
    let mut writer = fixture.open();
    let assets = writer.asset_dir().to_path_buf();
    fixture.assert_lock_held();
    let output = writer
        .run(&fixture.command(&assets, "clean"), Duration::from_secs(3))
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"actual stdout\n");
    assert_eq!(output.stderr, b"actual stderr\n");
    assert_eq!(fixture.effects(), 1);
    assert!(assets.join("input").exists());
    fixture.assert_lock_held();
    let record: Value = serde_json::from_reader(File::open(&fixture.lock).unwrap()).unwrap();
    assert!(record["process"].is_null());
    assert_ne!(record["owner"]["pid"], std::process::id());
    fixture.release(writer, &assets);
}

#[test]
fn actual_helper_stops_fd_closing_descendant_even_when_root_exited_zero() {
    let fixture = Fixture::new();
    let mut writer = fixture.open();
    let assets = writer.asset_dir().to_path_buf();
    let error = writer
        .run(&fixture.command(&assets, "descendant"), Duration::from_secs(3))
        .unwrap_err();
    let receipt = fixture.assert_stopped(&error);
    assert_eq!(
        receipt["exit_code"], 0,
        "actual root status must survive descendant intervention"
    );
    assert!(fixture.root.join("root-exit").exists());
    fixture.assert_no_next_stage(&mut writer);
    fixture.assert_lock_held();
    fixture.release(writer, &assets);
}

#[test]
fn actual_sdk_cancellation_waits_for_stopped_receipt_and_keeps_live_assets() {
    let fixture = Fixture::new();
    let mut writer = fixture.open();
    let assets = writer.asset_dir().to_path_buf();
    let observed = Cell::new(false);
    let cancel = || {
        if observed.get() {
            return true;
        }
        if fixture.heartbeat() == 0 {
            return false;
        }
        assert!(assets.join("input").exists());
        fixture.assert_lock_held();
        observed.set(true);
        true
    };
    let error = writer
        .run_cancellable(&fixture.command(&assets, "hang"), Duration::from_secs(3), &cancel)
        .unwrap_err();
    assert!(observed.get(), "cancel only after actual body dispatch");
    fixture.assert_stopped(&error);
    fixture.assert_no_next_stage(&mut writer);
    fixture.release(writer, &assets);
}

#[test]
fn actual_sdk_deadline_preserves_stopped_evidence_and_prohibits_replay() {
    let fixture = Fixture::new();
    let mut writer = fixture.open();
    let assets = writer.asset_dir().to_path_buf();
    let observed = Cell::new(false);
    let observe = || {
        if !observed.get() && fixture.heartbeat() > 0 {
            assert!(assets.join("input").exists());
            fixture.assert_lock_held();
            observed.set(true);
        }
        false
    };
    let started = Instant::now();
    let error = writer
        .run_cancellable(&fixture.command(&assets, "hang"), Duration::from_millis(400), &observe)
        .unwrap_err();
    assert!(started.elapsed() < LIMIT);
    assert!(observed.get());
    fixture.assert_stopped(&error);
    fixture.assert_no_next_stage(&mut writer);
    fixture.release(writer, &assets);
}

struct RawHelper {
    child: Child,
    input: Option<ChildStdin>,
    frames: Receiver<Value>,
}

impl RawHelper {
    fn start(fixture: &Fixture) -> Self {
        Self::start_with(fixture, |_| {})
    }

    fn start_with(fixture: &Fixture, configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(HELPER);
        command
            .arg("writer")
            .arg(&fixture.root)
            .arg(&fixture.lock)
            .arg("3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure(&mut command);
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(frame) = serde_json::from_str(&line) else { break };
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self { child, input, frames }
    }

    fn frame(&self) -> Value {
        self.frames.recv_timeout(LIMIT).unwrap()
    }

    fn send(&mut self, frame: &Value) {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, frame).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }

    fn wait(&mut self) {
        let mut status = None;
        wait_until(|| {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        assert!(status.unwrap().success());
    }
}

impl Drop for RawHelper {
    fn drop(&mut self) {
        // EOF requests cleanup. Never signal H while it owns a payload; every
        // fixture payload also has a five-second self-expiry if a test fails.
        drop(self.input.take());
        let deadline = Instant::now() + LIMIT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn raw_writer_stage_keeps_the_selected_owner_directory() {
    use lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV;

    let fixture = Fixture::new();
    let registry = fixture.root.join("owners");
    for changed in [Value::Null, json!(fixture.root.join("other-owners"))] {
        let mut helper = RawHelper::start_with(&fixture, |command| {
            command.env(OWNERS_DIRECTORY_ENV, &registry);
        });
        assert_eq!(helper.frame()["state"], "locked");
        helper.send(&json!({
            "action":"run", "command":["/usr/bin/printenv", OWNERS_DIRECTORY_ENV],
            "env":{(OWNERS_DIRECTORY_ENV):registry}, "timeout":1.0,
        }));
        assert_eq!(helper.frame()["state"], "started");
        let stopped = helper.frame();
        assert_eq!(stopped["state"], "stopped");
        assert_eq!(stopped["exit_code"], 0);
        assert_eq!(stopped["stdout"], format!("{}\n", registry.display()));

        let marker = fixture.root.join("must-not-run");
        helper.send(&json!({
            "action":"run", "command":["/usr/bin/touch", marker],
            "env":{(OWNERS_DIRECTORY_ENV):changed}, "timeout":1.0,
        }));
        assert!(matches!(
            helper.frames.recv_timeout(LIMIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(!helper.child.wait().unwrap().success());
        assert!(!marker.exists());
    }
    assert!(!registry.exists());
}

#[test]
fn actual_cli_eof_emits_provisional_then_terminal_stopped_no_result() {
    let fixture = Fixture::new();
    let mut helper = RawHelper::start(&fixture);
    let locked = helper.frame();
    assert_eq!(locked["state"], "locked");
    assert_eq!(locked["owner"]["pid"], helper.child.id());
    let assets = PathBuf::from(locked["asset_dir"].as_str().unwrap());
    let command = fixture.command(&assets, "hang");
    let mut arguments = vec![command.get_program().to_str().unwrap().to_owned()];
    arguments.extend(command.get_args().map(|arg| arg.to_str().unwrap().to_owned()));
    helper.send(&json!({"action":"run", "command":arguments, "timeout":3.0}));
    let started = helper.frame();
    assert_eq!(started["state"], "started");
    assert_eq!(started["owner"], locked["owner"]);
    assert_ne!(started["process"]["pid"], helper.child.id());
    wait_until(|| fixture.heartbeat() > 0);
    assert!(assets.join("input").exists());
    fixture.assert_lock_held();
    drop(helper.input.take());
    let provisional = helper.frame();
    assert_eq!(provisional["state"], "still_running");
    assert_eq!(provisional["result"], "no_result");
    let stopped = helper.frame();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["process"], started["process"]);
    assert_eq!(stopped["owner"]["pid"], helper.child.id());
    assert_eq!(stopped["result"], "no_result");
    assert_eq!(stopped["intervened"], true);
    assert_eq!(stopped["owned_work_stopped"], true);
    let pid = u32::try_from(stopped["process"]["pid"].as_u64().unwrap()).unwrap();
    assert!(!group_present(pid).unwrap());
    let before = fixture.heartbeat();
    thread::sleep(Duration::from_millis(40));
    assert_eq!(fixture.heartbeat(), before);
    assert_eq!(fixture.effects(), 1);
    assert!(!fixture.root.join("asset-lost").exists());
    helper.wait();
    wait_until(|| !assets.exists() && fixture.lock_available());
}

#[test]
fn actual_helper_output_flood_returns_bounded_stopped_no_result() {
    let fixture = Fixture::new();
    let mut writer = fixture.open();
    let assets = writer.asset_dir().to_path_buf();
    let error = writer
        .run(&fixture.command(&assets, "flood"), Duration::from_secs(3))
        .unwrap_err();
    let receipt = fixture.assert_stopped(&error);
    let stdout = receipt["stdout"].as_str().unwrap();
    let stderr = receipt["stderr"].as_str().unwrap();
    assert!(!stdout.is_empty());
    assert!(stdout.len() <= CAPTURE_LIMIT);
    assert!(stderr.len() <= CAPTURE_LIMIT);
    assert!(receipt["reason"].as_str().unwrap().contains("output limit"));
    fixture.assert_no_next_stage(&mut writer);
    fixture.release(writer, &assets);
}

/// This owner intentionally has no stdout reader thread. After two small
/// handshake frames its live read descriptor remains open and unread.
struct NonReadingClient {
    child: Child,
    input: Option<ChildStdin>,
    output: io::BufReader<std::process::ChildStdout>,
}

impl NonReadingClient {
    fn start(fixture: &Fixture) -> Self {
        let mut child = Command::new(HELPER)
            .arg("writer")
            .arg(&fixture.root)
            .arg(&fixture.lock)
            .arg("3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        rustix::fs::fcntl_setfl(
            &output,
            rustix::fs::fcntl_getfl(&output).unwrap() | rustix::fs::OFlags::NONBLOCK,
        )
        .unwrap();
        Self {
            child,
            input,
            output: io::BufReader::new(output),
        }
    }

    fn frame(&mut self) -> Value {
        let started = Instant::now();
        let mut line = String::new();
        loop {
            match self.output.read_line(&mut line) {
                Ok(0) => panic!("helper stdout closed before handshake"),
                Ok(_) if line.ends_with('\n') => return serde_json::from_str(&line).unwrap(),
                Ok(_) => {}
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                Err(error) => panic!("helper handshake read failed: {error}"),
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "helper handshake deadline expired"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn send(&mut self, frame: &Value) {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, frame).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }
}

impl Drop for NonReadingClient {
    fn drop(&mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + LIMIT;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        // The output descriptor remains open throughout this wait. Cleanup
        // never depends on signaling H or making the client resume reading.
    }
}

#[test]
fn actual_helper_bounds_terminal_delivery_when_client_keeps_stdout_open_unread() {
    use std::os::unix::fs::MetadataExt as _;

    let fixture = Fixture::new();
    let mut client = NonReadingClient::start(&fixture);
    let locked = client.frame();
    assert_eq!(locked["state"], "locked");
    assert_eq!(locked["owner"]["pid"], client.child.id());
    let assets = PathBuf::from(locked["asset_dir"].as_str().unwrap());
    let identity = fs::metadata(&fixture.lock).unwrap();
    let inode = (identity.dev(), identity.ino());
    let command = fixture.command(&assets, "nul-flood");
    let mut arguments = vec![command.get_program().to_str().unwrap().to_owned()];
    arguments.extend(command.get_args().map(|argument| argument.to_str().unwrap().to_owned()));
    client.send(&json!({"action":"run", "command":arguments, "timeout":3.0}));
    let started = client.frame();
    assert_eq!(started["state"], "started");
    assert_eq!(started["owner"], locked["owner"]);
    let payload = u32::try_from(started["process"]["pid"].as_u64().unwrap()).unwrap();
    assert_ne!(payload, client.child.id());
    assert_eq!(started["process"]["process_group"], payload);
    assert!(client.input.is_some(), "keep the control connection open as well");

    // Never read stdout again: two 64 KiB NUL captures expand to about 0.8 MiB
    // of JSON, exceeding a pipe's capacity while remaining a legal frame.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = client.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "helper remained blocked on its unread terminal reply"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert!(
        !status.success(),
        "the undeliverable terminal reply must report a CLI delivery failure"
    );
    assert!(!group_present(payload).unwrap());
    assert_eq!(fixture.effects(), 1);
    assert!(!assets.exists());
    let identity = fs::metadata(&fixture.lock).unwrap();
    assert_eq!(
        (identity.dev(), identity.ino()),
        inode,
        "capacity recovery must retain the stable lock inode"
    );
    assert!(fixture.lock_available());
}

#[test]
fn sdk_requires_explicit_usable_result_attestation_from_an_older_helper() {
    use std::os::unix::fs::PermissionsExt as _;
    const LEGACY_HELPER: &str = r"#!/usr/bin/env python3
import json, os, pathlib, sys
os.setsid()
owner = dict(version=1, pid=os.getpid(), session=os.getpid(), start='fixture-birth')
loans = json.loads(os.environ['LEAN_HOST_MCP_WRITER_LOCK_LOANS'])
assets = pathlib.Path(sys.argv[2]) / 'legacy-assets'
assets.mkdir()
print(json.dumps(dict(state='locked', owner=owner, asset_dir=str(assets), lock_loans=[dict(path=p['path'],device=p['device'],inode=p['inode']) for p in loans])),flush=True)
command = json.loads(sys.stdin.readline())
frame = dict(state='stopped', owner=owner, process=dict(pid=os.getpid()+1,process_group=os.getpid()+1,session=os.getpid(),start='fixture-command'),owned_work_stopped=True, exit_code=0,stdout='unattested',stderr='')
mode = command['command'][-1]
if mode != 'missing-result': frame['result']='result'
if mode != 'missing-flags': frame.update(cancelled=False,intervened=False)
if mode == 'malformed-flags': frame['intervened']='false'
print(json.dumps(frame),flush=True)
sys.stdin.read()
";
    for mode in ["missing-result", "missing-flags", "malformed-flags"] {
        let fixture = Fixture::new();
        let helper = fixture.root.join("legacy-helper");
        fs::write(&helper, LEGACY_HELPER).expect("write private protocol fixture");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("executable protocol fixture");
        let mut writer = SharedWriter::open(&helper, &fixture.root, LIMIT).expect("bound legacy helper");
        let mut command = Command::new("never-executed");
        command.arg(mode);
        let error = writer
            .run(&command, Duration::from_secs(1))
            .expect_err("unattested bytes must never become Output");
        let failure = WriterFailure::from_io(&error).expect("typed stop evidence");
        assert_eq!(failure.disposition, WriterDisposition::Stopped);
        assert!(failure.stop_receipt.is_some());
        assert!(
            writer.run(&command, Duration::from_secs(1)).is_err(),
            "unusable result seals the scope"
        );
    }
}

#[test]
fn native_registry_blocks_actual_sdk_admission_and_raw_helper_go_until_owned_group_stops() {
    use lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV;
    let directory = tempfile::tempdir().expect("private native registry fixture");
    let registry = directory.path().canonicalize().unwrap().join("native-owners");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "native_registry_fence_fixture", "--nocapture"])
        .env("LEAN_WRITER_NATIVE_FENCE_FIXTURE", "1")
        .env(OWNERS_DIRECTORY_ENV, registry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("private namespace test process");
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() >= LIMIT {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("native writer fence fixture exceeded deadline: {output:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().expect("reap private test process");
    assert!(
        output.status.success(),
        "native writer fence fixture failed: {output:?}"
    );
}

#[test]
fn native_registry_fence_fixture() {
    if std::env::var_os("LEAN_WRITER_NATIVE_FENCE_FIXTURE").is_none() {
        return;
    }
    use lean_toolchain::native_guardian::{NativeProcessIdentity, registry_directory, reserve_owner_record};
    use std::os::unix::fs::MetadataExt as _;
    struct NativeRoot(Child);
    impl Drop for NativeRoot {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _kill = self.0.kill();
            }
            let _reap = self.0.wait();
        }
    }
    const NATIVE: &str = r"
import os, pathlib, signal, sys, time
root=pathlib.Path(sys.argv[1]); os.setsid(); signal.alarm(5)
def ready(name, value):
    temporary=root/(name+'.new'); temporary.write_text(value); temporary.replace(root/name)
os.closerange(3,4096)
guardian=os.fork()
if guardian == 0:
    signal.alarm(5)
    os.setpgid(0,0)
    ready('guardian-ready', str(os.getpid()))
    while not (root/'end-guardian').exists(): time.sleep(.002)
    os._exit(0)
ready('native-ready', str(os.getpid()))
while not (root/'end-guardian').exists(): time.sleep(.002)
os.waitpid(guardian,0)
ready('guardian-reaped', 'yes')
with (root/'native-heartbeat').open('ab',buffering=0) as f:
    while True: f.write(b'x'); time.sleep(.005)
";
    let fixture = Fixture::new();
    let mut helper = RawHelper::start(&fixture);
    assert_eq!(helper.frame()["state"], "locked");
    let mut native = NativeRoot(
        Command::new("python3")
            .args(["-c", NATIVE])
            .arg(&fixture.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until(|| fixture.root.join("native-ready").exists() && fixture.root.join("guardian-ready").exists());
    let process = NativeProcessIdentity::capture(native.0.id())
        .unwrap()
        .expect("actual owned native root");
    let guardian_pid = fs::read_to_string(fixture.root.join("guardian-ready"))
        .unwrap()
        .parse()
        .unwrap();
    let guardian = NativeProcessIdentity::capture(guardian_pid)
        .unwrap()
        .expect("actual owned guardian scope");
    fs::write(fixture.root.join("end-guardian"), b"stop exact owned guardian").unwrap();
    wait_until(|| {
        fixture.root.join("guardian-reaped").exists()
            && fs::metadata(fixture.root.join("native-heartbeat")).is_ok_and(|metadata| metadata.len() > 0)
    });
    let lock = File::open(&fixture.lock).unwrap();
    let metadata = lock.metadata().unwrap();
    let registry = registry_directory().unwrap();
    let nonce = format!("{:048x}", u64::from(std::process::id()));
    let record = reserve_owner_record(&registry, &nonce, Duration::from_secs(1)).unwrap();
    fs::write(
        &record,
        serde_json::to_vec(
            &json!({"version":2,"scope":"inherited-process-group-v2","phase":"running",
        "generation_id":nonce,"parent":NativeProcessIdentity::capture(std::process::id()).unwrap().unwrap(),
        "process":process,"guardian":guardian,"protected_domains":[{"project_root":fixture.root,
        "lock_path":fixture.lock,"device":metadata.dev(),"inode":metadata.ino(),"epoch":"fixture-read"}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let marker = fixture.root.join("must-not-go");
    helper.send(
        &json!({"action":"run","command":["/bin/sh","-c","printf bad > \"$1\"","fence-test",marker],"timeout":1.0}),
    );
    let mut status = None;
    wait_until(|| {
        status = helper.child.try_wait().unwrap();
        status.is_some()
    });
    assert!(
        !status.unwrap().success(),
        "raw H must refuse GO after native fence appears"
    );
    assert!(!marker.exists());
    assert!(
        matches!(
            helper.frames.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "raw H must not emit Started or a success result"
    );
    drop(helper);
    let exclusive = OpenOptions::new().read(true).write(true).open(&fixture.lock).unwrap();
    exclusive
        .try_lock()
        .expect("guardian death made the kernel lock obtainable");
    drop(exclusive);
    let error = SharedWriter::open(Path::new(HELPER), &fixture.root, Duration::from_millis(80))
        .err()
        .expect("actual SDK admission must still be fenced by the live owned native root");
    assert_eq!(
        WriterFailure::from_io(&error).unwrap().disposition,
        WriterDisposition::NotStarted
    );
    assert!(native.0.try_wait().unwrap().is_none());
    native.0.kill().expect("kill this test's exact owned native Child");
    native.0.wait().unwrap();
    assert!(!group_present(native.0.id()).unwrap());
    let mut writer = fixture.open();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "printf recovered > \"$1\"", "fence-test"])
        .arg(&marker);
    assert!(writer.run(&command, Duration::from_secs(1)).unwrap().status.success());
    assert_eq!(fs::read(&marker).unwrap(), b"recovered");
}
