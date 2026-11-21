//! The actual generic native-reader command keeps custody across payload loss.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lean_toolchain::shared_writer::project_lock_path;
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(5);
const PAYLOAD: &str = r"#!/usr/bin/env python3
import os, signal, subprocess, sys, time
from pathlib import Path
root = Path(__file__).parent
if os.environ.get('NATIVE_READER_CANCEL_ID'):
 expected=tuple(map(int,os.environ['NATIVE_READER_CANCEL_ID'].split(':')))
 for fd in range(128):
  try: st=os.fstat(fd)
  except OSError: continue
  assert (st.st_dev,st.st_ino) != expected, 'private cancellation pipe leaked to payload'
signal.alarm(8)
if os.environ.get('NATIVE_READER_STDIN_FIXTURE'):
    (root / 'received-stdin').write_bytes(sys.stdin.buffer.read())
(root / 'payload.pending').write_text(str(os.getpid()))
(root / 'payload.pending').replace(root / 'payload')
if (root / 'descendant-mode').exists():
    leaf = '''import os,signal,time,sys
from pathlib import Path
root=Path(sys.argv[1]); signal.alarm(8)
for fd in range(0,4096):
 try: os.close(fd)
 except OSError: pass
(root/'leaf.pending').write_text(str(os.getpid()))
(root/'leaf.pending').replace(root/'leaf')
while True:
 (root/'heartbeat').write_text(str(time.monotonic_ns()))
 time.sleep(.01)
'''
    subprocess.Popen([sys.executable, '-c', leaf, str(root)], close_fds=True)
    while not (root/'leaf').exists(): time.sleep(.01)
while not (root/'go').exists(): time.sleep(.01)
if os.environ.get('NATIVE_READER_VERBOSE_FIXTURE'): print('x'*1024)
print('payload result')
";

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    dependency: PathBuf,
    executable: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("Project");
        let dependency = root.join("Dependency");
        fs::create_dir_all(project.join(".lake")).unwrap();
        fs::create_dir_all(dependency.join(".lake")).unwrap();
        fs::write(dependency.join("lake-manifest.json"), "{\"packages\":[]}").unwrap();
        fs::write(
            project.join("lake-manifest.json"),
            serde_json::to_vec(&json!({
                "version":"1.1.0", "packagesDir":".lake/packages", "packages":[{
                    "name":"dependency", "type":"path", "dir":"../Dependency", "manifestFile":"lake-manifest.json"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let executable = root.join("payload.py");
        fs::write(&executable, PAYLOAD).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            directory,
            root,
            project,
            dependency,
            executable,
        }
    }
    fn command(&self, timeout_millis: u64) -> Command {
        self.reader_command(timeout_millis, false)
    }
    fn reader_command(&self, timeout_millis: u64, stdout_frame: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        command.arg("native-reader");
        if stdout_frame {
            command.arg("--stdout-result-frame");
        }
        command
            .arg("--project-root")
            .arg(&self.project)
            .arg("--capacity-limit")
            .arg("1")
            .arg("--timeout-millis")
            .arg(timeout_millis.to_string())
            .arg("--max-output-bytes")
            .arg("64")
            .arg("--")
            .arg(&self.executable)
            .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", self.root.join("owners"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn lock(root: &Path) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(project_lock_path(root).unwrap())
            .unwrap()
    }
    fn marker_pid(&self, name: &str) -> rustix::process::Pid {
        let pid: i32 = fs::read_to_string(self.root.join(name)).unwrap().parse().unwrap();
        rustix::process::Pid::from_raw(pid).unwrap()
    }
}

struct OwnedChild(Option<Child>);
impl OwnedChild {
    fn spawn(mut command: Command) -> Self {
        Self(Some(command.spawn().unwrap()))
    }
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
    fn finish(mut self) -> (std::process::ExitStatus, Value) {
        wait_for(|| self.child().try_wait().unwrap().is_some());
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid native-reader result {error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status, value)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.kill());
            drop(child.wait());
        }
    }
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now().checked_add(LIMIT).unwrap();
    while !predicate() {
        assert!(Instant::now() < deadline, "bounded native-reader fixture transition");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn payload_starts_only_after_guardian_custody_of_complete_canonical_union() {
    let fixture = Fixture::new();
    let child = OwnedChild::spawn(fixture.command(3000));
    wait_for(|| fixture.root.join("payload").exists());
    for root in [&fixture.project, &fixture.dependency] {
        assert!(
            Fixture::lock(root).try_lock().is_err(),
            "writer entered live reader {root:?}"
        );
    }
    let records = fs::read_dir(fixture.root.join("owners"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("owner.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    let owner: Value = serde_json::from_slice(&fs::read(&records[0]).unwrap()).unwrap();
    assert_eq!(owner["protected_domains"].as_array().unwrap().len(), 2);
    fs::write(fixture.root.join("go"), "finish").unwrap();
    let (status, result) = child.finish();
    assert!(status.success(), "{result}");
    assert_eq!(result["state"], "completed");
    assert_eq!(result["scope_closed"], true);
    assert_eq!(result["target_started"], true);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"], "payload result\n");
    assert_eq!(result["runtime_compatibility"]["status"], "not_observed");
    for root in [&fixture.project, &fixture.dependency] {
        assert!(Fixture::lock(root).try_lock().is_ok());
    }
}

#[test]
fn descriptor_closing_descendant_is_stopped_before_read_custody_releases() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("descendant-mode"), "fork").unwrap();
    let child = OwnedChild::spawn(fixture.command(3000));
    wait_for(|| fixture.root.join("heartbeat").exists());
    let leaf = fixture.marker_pid("leaf");
    assert!(Fixture::lock(&fixture.dependency).try_lock().is_err());
    fs::write(fixture.root.join("go"), "finish root").unwrap();
    let (status, result) = child.finish();
    assert!(
        !status.success(),
        "forced descendant cleanup cannot authorize a usable result: {result}"
    );
    assert_eq!(result["state"], "stopped");
    assert_eq!(result["intervened"], true);
    assert_eq!(result["scope_closed"], true);
    wait_for(|| rustix::process::test_kill_process(leaf).is_err());
    let heartbeat = fs::read(fixture.root.join("heartbeat")).unwrap();
    assert!(Fixture::lock(&fixture.dependency).try_lock().is_ok());
    thread::sleep(Duration::from_millis(30));
    assert_eq!(fs::read(fixture.root.join("heartbeat")).unwrap(), heartbeat);
}

#[cfg(target_os = "linux")]
#[test]
fn detached_survivor_at_verdict_refuses_success() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.executable,
        r"#!/usr/bin/env python3
import os, signal, time
from pathlib import Path
root = Path(__file__).parent
if os.fork() == 0:
    os.setsid()
    os.closerange(0, 4096)
    signal.alarm(8)
    (root/'detached').touch()
    while not (root/'release').exists(): time.sleep(.01)
    (root/'finished').touch()
    os._exit(0)
while not (root/'detached').exists(): time.sleep(.01)
print('normal payload exit')
",
    )
    .unwrap();
    let (status, result) = OwnedChild::spawn(fixture.command(3000)).finish();
    // The escaped fixture owns its bounded exit; group cleanup cannot certify it.
    fs::write(fixture.root.join("release"), "").unwrap();
    wait_for(|| fixture.root.join("finished").exists());
    assert!(!status.success(), "{result}");
    assert_eq!(result["state"], "stopped");
    assert_eq!(result["intervened"], true);
    assert_eq!(result["exit_code"], 0);
}

#[cfg(target_os = "linux")]
#[test]
fn lost_waiter_before_report_cannot_publish_payload_success() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.executable,
        r"#!/usr/bin/env python3
import os, signal
from pathlib import Path
root = Path(__file__).parent
pid = os.getppid()
args = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\x00')
assert args[1] == b'native-reader-wait'
(root/'started').open('x').close()
os.kill(pid, signal.SIGKILL)
print('not a completion report')
",
    )
    .unwrap();
    let (status, result) = OwnedChild::spawn(fixture.command(3000)).finish();
    assert!(fixture.root.join("started").exists());
    assert!(!status.success(), "{result}");
    assert_eq!(result["scope_closed"], true);
    assert_eq!(result["target_started"], Value::Null);
    assert_eq!(result["exit_status"], Value::Null);
    assert!(result["detail"].as_str().unwrap().contains("waiter exited"));
}

#[test]
fn deadline_stops_scope_without_returning_semantic_success_or_replaying() {
    let fixture = Fixture::new();
    let child = OwnedChild::spawn(fixture.command(500));
    wait_for(|| fixture.root.join("payload").exists());
    let payload = fixture.marker_pid("payload");
    let (status, result) = child.finish();
    assert!(!status.success());
    assert_eq!(result["state"], "stopped");
    assert_eq!(result["scope_closed"], true);
    assert!(result["exit_code"].is_null());
    wait_for(|| rustix::process::test_kill_process(payload).is_err());
    assert!(Fixture::lock(&fixture.project).try_lock().is_ok());
}

#[test]
fn controller_death_does_not_drop_read_custody_while_payload_survives() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("descendant-mode"), "fork").unwrap();
    let mut child = OwnedChild::spawn(fixture.command(3000));
    wait_for(|| fixture.root.join("heartbeat").exists());
    let payload = fixture.marker_pid("payload");
    let leaf = fixture.marker_pid("leaf");
    child.child().kill().unwrap();
    child.child().wait().unwrap();
    wait_for(|| {
        rustix::process::test_kill_process(payload).is_err() && rustix::process::test_kill_process(leaf).is_err()
    });
    wait_for(|| Fixture::lock(&fixture.project).try_lock().is_ok());
}

#[test]
fn failed_dependency_read_admission_never_starts_the_payload() {
    let fixture = Fixture::new();
    let writer = Fixture::lock(&fixture.dependency);
    writer.lock().unwrap();
    let child = OwnedChild::spawn(fixture.command(250));
    let (status, result) = child.finish();
    assert!(!status.success());
    assert_ne!(result["state"], "completed");
    assert!(!fixture.root.join("payload").exists());
    assert_eq!(result["scope_closed"], true);
}

#[test]
fn bootstrap_without_authenticated_parent_never_executes_target() -> io::Result<()> {
    let fixture = Fixture::new();
    let mut command = fixture.command(200);
    // Rebuild the same arguments under the private bootstrap command.
    let args = command
        .get_args()
        .skip(1)
        .map(std::ffi::OsStr::to_os_string)
        .collect::<Vec<_>>();
    command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
    command
        .arg("native-reader-exec")
        .args(args)
        .env_remove("LEAN_RS_WORKER_PARENT_SOCKET")
        .env_remove("LEAN_RS_WORKER_PARENT_NONCE")
        .stdout(Stdio::piped());
    let output = command.output()?;
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["target_started"], false);
    assert!(!fixture.root.join("payload").exists());
    Ok(())
}

#[test]
fn invoking_caller_exit_stops_external_reader_without_killing_other_scopes() {
    let fixture = Fixture::new();
    let command = fixture.command(3000);
    let mut argv = vec![command.get_program().to_string_lossy().into_owned()];
    argv.extend(command.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    let caller = r"import json,os,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1])
env=os.environ.copy(); env['LEAN_RS_WORKER_OWNERS_DIRECTORY']=str(root/'owners')
with (root/'reader-result').open('wb') as output:
 child=subprocess.Popen(json.loads(sys.argv[2]),env=env,stdout=output,stderr=subprocess.DEVNULL)
 (root/'controller.pending').write_text(str(child.pid))
 (root/'controller.pending').replace(root/'controller')
 while not (root/'caller-exit').exists(): time.sleep(.01)
os._exit(0)
";
    let mut caller_command = Command::new("/usr/bin/python3");
    caller_command
        .args(["-c", caller])
        .arg(&fixture.root)
        .arg(serde_json::to_string(&argv).unwrap());
    let mut caller = OwnedChild::spawn(caller_command);
    wait_for(|| fixture.root.join("payload").exists());
    let payload = fixture.marker_pid("payload");
    fs::write(fixture.root.join("caller-exit"), "caller ended").unwrap();
    caller.child().wait().unwrap();
    let result_path = fixture.root.join("reader-result");
    wait_for(|| fs::read(&result_path).is_ok_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_ok()));
    let result: Value = serde_json::from_slice(&fs::read(result_path).unwrap()).unwrap();
    assert_eq!(result["state"], "stopped");
    assert_eq!(result["scope_closed"], true);
    wait_for(|| rustix::process::test_kill_process(payload).is_err());
    assert!(Fixture::lock(&fixture.project).try_lock().is_ok());
}

#[test]
fn frame_requires_complete_stdout_but_accepts_clipped_diagnostics() {
    for (frame, overflow_stdout, expected) in [
        (false, false, "stopped"),
        (true, false, "completed"),
        (true, true, "stopped"),
    ] {
        let fixture = Fixture::new();
        fs::write(
            &fixture.executable,
            "#!/bin/sh\nprintf '%0100d' 0 >&2\nprintf '%s' \"$RESULT\"\n",
        )
        .unwrap();
        let mut command = fixture.reader_command(3000, frame);
        let output = if overflow_stdout {
            "x".repeat(65)
        } else {
            "valid".into()
        };
        command.env("RESULT", &output);
        let (status, result) = OwnedChild::spawn(command).finish();
        assert_eq!(status.success(), expected == "completed");
        assert_eq!(result["state"], expected);
        assert_eq!(result["scope_closed"], true);
        assert_eq!(result["output_truncated"], expected != "completed");
        assert_eq!(result["stderr"].as_str().unwrap().len(), 64);
        if expected == "completed" {
            assert_eq!(result["stdout"], output);
            assert_eq!(result["detail"], "payload stderr diagnostic clipped");
        }
    }
}

#[test]
fn truncated_output_cannot_be_returned_as_a_usable_success() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("go"), "finish").unwrap();
    let mut command = fixture.command(3000);
    command.env("NATIVE_READER_VERBOSE_FIXTURE", "1");
    let (status, result) = OwnedChild::spawn(command).finish();
    assert!(!status.success());
    assert_eq!(result["state"], "stopped");
    assert_eq!(result["scope_closed"], true);
    assert_eq!(result["output_truncated"], true);
    assert_eq!(result["exit_code"], 0);
}

fn stdin_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut digest, "{byte:02x}").unwrap();
    }
    digest
}

#[test]
fn explicit_stdin_offers_exact_bytes_after_custody_and_reports_the_same_receipt() {
    use std::io::Write as _;
    let fixture = Fixture::new();
    fs::write(fixture.root.join("go"), "finish").unwrap();
    let original = fixture.command(3000);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
    command
        .arg("native-reader")
        .arg("--stdin")
        .args(original.get_args().skip(1))
        .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", fixture.root.join("owners"))
        .env("NATIVE_READER_STDIN_FIXTURE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command);
    let bytes = b"import Test\n#print axioms value\n";
    child.child().stdin.take().unwrap().write_all(bytes).unwrap();
    let (status, result) = child.finish();
    assert!(status.success(), "{result}");
    assert_eq!(result["state"], "completed");
    assert_eq!(result["stdin_sha256"], stdin_digest(bytes));
    assert_eq!(result["stdin_bytes"], u64::try_from(bytes.len()).unwrap());
    assert_eq!(fs::read(fixture.root.join("received-stdin")).unwrap(), bytes);
    assert_eq!(result["runtime_compatibility"]["status"], "not_observed");
}

#[test]
fn default_payload_stdin_is_closed_and_has_no_input_receipt() {
    use std::io::Write as _;
    let fixture = Fixture::new();
    fs::write(fixture.root.join("go"), "finish").unwrap();
    let mut command = fixture.command(3000);
    command.stdin(Stdio::piped()).env("NATIVE_READER_STDIN_FIXTURE", "1");
    let mut child = OwnedChild::spawn(command);
    child.child().stdin.take().unwrap().write_all(b"not opted in").unwrap();
    let (status, result) = child.finish();
    assert!(status.success(), "{result}");
    assert!(result.get("stdin_sha256").is_none());
    assert!(result.get("stdin_bytes").is_none());
    assert!(fs::read(fixture.root.join("received-stdin")).unwrap().is_empty());
}

#[test]
fn missing_stdin_eof_refuses_before_any_native_bootstrap_or_payload() {
    let fixture = Fixture::new();
    let original = fixture.command(150);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
    command
        .arg("native-reader")
        .arg("--stdin")
        .args(original.get_args().skip(1))
        .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", fixture.root.join("owners"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command);
    let _held_writer = child.child().stdin.take().unwrap();
    let (status, result) = child.finish();
    assert!(!status.success());
    assert_eq!(result["state"], "not_started");
    assert_eq!(result["target_started"], false);
    assert_eq!(result["scope_closed"], true);
    assert!(!fixture.root.join("payload").exists());
}

#[test]
fn stdin_offer_from_another_directory_is_refused_before_payload() {
    use lean_rs_worker_parent::{LeanWorkerNativeScope, LeanWorkerNativeSpawnError};
    use lean_toolchain::native_guardian::NativeCapacitySlot;
    let mut fixture = Fixture::new();
    let registry = fixture.root.join("owners");
    let slot = NativeCapacitySlot {
        key: "foreign-input-test".into(),
        scope: "main".into(),
        limit: 1,
    };
    let scope = LeanWorkerNativeScope::prepare(&registry, LIMIT, Some(&slot)).unwrap();
    let _owned_inputs = scope.input_directory().unwrap();
    let foreign = fixture.root.join("foreign");
    fs::create_dir(&foreign).unwrap();
    let bytes = b"outside the authenticated generation";
    fs::write(foreign.join("stdin"), bytes).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
    command
        .arg("native-reader-exec")
        .arg("--project-root")
        .arg(&fixture.project)
        .args(["--capacity-limit", "1", "--timeout-millis", "3000", "--stdin", "--"])
        .arg(&fixture.executable)
        .env(
            "LEAN_RS_WORKER_GUARDIAN_EXECUTABLE",
            Path::new(env!("CARGO_BIN_EXE_lean-host-mcp")).canonicalize().unwrap(),
        )
        .env(
            "LEAN_HOST_MCP_NATIVE_READER_STDIN",
            serde_json::to_string(&json!({
                "path":foreign.join("stdin"),"sha256":stdin_digest(bytes),"bytes":bytes.len()
            }))
            .unwrap(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // An assertion or ACK error after possible spawn must leave the registry
    // and staged inputs available to SDK cleanup and the failed-test observer.
    fixture.directory.disable_cleanup(true);
    let mut lease = match scope.spawn(&mut command, LIMIT) {
        Ok(lease) => lease,
        Err(LeanWorkerNativeSpawnError::BeforeSpawn(source)) => panic!("reader bootstrap did not spawn: {source}"),
        Err(LeanWorkerNativeSpawnError::AfterSpawn { source, pending }) => {
            drop(pending); // SDK retains cleanup; no closure claim.
            panic!(
                "reader bootstrap failed after spawn: {source}; preserved {}",
                fixture.root.display()
            );
        }
    };
    let stdout = lease.take_stdout().unwrap();
    let mut stderr = lease.take_stderr().unwrap();
    lease.acknowledge().unwrap();
    wait_for(|| lease.try_wait().unwrap().is_some());
    wait_for(|| lease.custody_closed().unwrap());
    fixture.directory.disable_cleanup(false);
    let result: Value = serde_json::from_reader(stdout).unwrap_or_else(|error| {
        let mut diagnostics = String::new();
        io::Read::read_to_string(&mut stderr, &mut diagnostics).unwrap();
        panic!("invalid native-reader result: {error}: stderr={diagnostics}");
    });
    assert_eq!(result["target_started"], false);
    assert!(
        result["detail"]
            .as_str()
            .unwrap()
            .contains("outside the authenticated generation")
    );
    assert!(!fixture.root.join("payload").exists());
}

#[test]
fn invoking_caller_loss_during_stdin_capture_refuses_even_when_eof_is_held_open() {
    let fixture = Fixture::new();
    let command = fixture.command(5000);
    let mut argv = vec![
        command.get_program().to_string_lossy().into_owned(),
        "native-reader".into(),
        "--stdin".into(),
    ];
    argv.extend(
        command
            .get_args()
            .skip(1)
            .map(|argument| argument.to_string_lossy().into_owned()),
    );
    let caller = r"import json,os,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]); env=os.environ.copy(); env['LEAN_RS_WORKER_OWNERS_DIRECTORY']=str(root/'owners')
with (root/'reader-result').open('wb') as output:
 child=subprocess.Popen(json.loads(sys.argv[2]),env=env,stdin=0,stdout=output,stderr=subprocess.DEVNULL)
deadline=time.monotonic()+4
while not any((root/'owners').glob('worker-*/inputs')):
 if time.monotonic()>deadline: sys.exit(77)
 time.sleep(.005)
os._exit(0)
";
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    let mut caller_command = Command::new("/usr/bin/python3");
    caller_command
        .args(["-c", caller])
        .arg(&fixture.root)
        .arg(serde_json::to_string(&argv).unwrap())
        .stdin(Stdio::from(std::fs::File::from(reader)));
    let started = Instant::now();
    let mut caller = OwnedChild::spawn(caller_command);
    rustix::io::write(&writer, b"import partial input").unwrap();
    assert!(caller.child().wait().unwrap().success());
    let output = fixture.root.join("reader-result");
    wait_for(|| fs::read(&output).is_ok_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_ok()));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "caller loss must resolve before the five-second deadline"
    );
    // The test driver still owns the only writer. EOF cannot explain the result.
    let result: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(result["state"], "not_started");
    assert_eq!(result["target_started"], false);
    assert_eq!(result["scope_closed"], true);
    assert!(!fixture.root.join("payload").exists());
    drop(writer);
}

#[test]
fn private_pipe_cancels_live_scope_while_its_python_caller_stays_alive() {
    for mode in ["byte", "eof"] {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("descendant-mode"), "fork").unwrap();
        let command = fixture.command(8000);
        let mut argv = vec![command.get_program().to_string_lossy().into_owned()];
        argv.extend(command.get_args().map(|arg| arg.to_string_lossy().into_owned()));
        let caller = r"import json,os,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]); argv=json.loads(sys.argv[2]); mode=sys.argv[3]
read,write=os.pipe(); os.set_blocking(read,False); os.set_inheritable(read,False)
st=os.fstat(read); env=os.environ.copy(); env['NATIVE_READER_CANCEL_ID']=str(st.st_dev)+':'+str(st.st_ino)
env['LEAN_RS_WORKER_OWNERS_DIRECTORY']=str(root/'owners')
separator=argv.index('--'); argv[separator:separator]=['--cancel-fd',str(read)]
child=subprocess.Popen(argv,env=env,pass_fds=(read,),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
os.close(read)
while not (root/'heartbeat').exists():
 assert child.poll() is None
 time.sleep(.01)
started=time.monotonic()
if mode=='byte': os.write(write,b'x')
os.close(write)
output,error=child.communicate(timeout=3)
assert time.monotonic()-started < 2, (output,error)
assert child.returncode != 0, (output,error)
sys.stdout.buffer.write(output)
";
        let mut driver = Command::new("/usr/bin/python3");
        driver
            .args(["-c", caller])
            .arg(&fixture.root)
            .arg(serde_json::to_string(&argv).unwrap())
            .arg(mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (status, result) = OwnedChild::spawn(driver).finish();
        assert!(status.success(), "{result}");
        assert_eq!(result["state"], "stopped");
        assert_eq!(result["scope_closed"], true);
        assert!(result["exit_status"].is_null());
        assert!(
            result["payload_usage"].is_null(),
            "no invented usage without an actual payload wait"
        );
        let root = fixture.marker_pid("payload");
        let leaf = fixture.marker_pid("leaf");
        wait_for(|| {
            rustix::process::test_kill_process(root).is_err() && rustix::process::test_kill_process(leaf).is_err()
        });
        assert!(Fixture::lock(&fixture.project).try_lock().is_ok());
    }
}

#[test]
fn explicit_pipe_cancellation_during_stdin_capture_is_unstarted_before_eof() {
    let fixture = Fixture::new();
    let command = fixture.command(8000);
    let mut argv = vec![command.get_program().to_string_lossy().into_owned()];
    argv.extend(command.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    let caller = r"import json,os,subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]); argv=json.loads(sys.argv[2])
read,write=os.pipe(); os.set_blocking(read,False); os.set_inheritable(read,False)
env=os.environ.copy(); env['LEAN_RS_WORKER_OWNERS_DIRECTORY']=str(root/'owners')
separator=argv.index('--'); argv[separator:separator]=['--stdin','--cancel-fd',str(read)]
child=subprocess.Popen(argv,env=env,pass_fds=(read,),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
os.close(read)
while not list((root/'owners').glob('worker-*/inputs')):
 assert child.poll() is None
 time.sleep(.01)
started=time.monotonic(); os.write(write,b'x')
# Keep the producer open until JSON arrives: EOF cannot explain cancellation.
output=child.stdout.readline()
assert time.monotonic()-started < 2, output
assert child.wait(timeout=2) != 0, output
child.stdin.close(); os.close(write)
sys.stdout.buffer.write(output)
";
    let mut driver = Command::new("/usr/bin/python3");
    driver
        .args(["-c", caller])
        .arg(&fixture.root)
        .arg(serde_json::to_string(&argv).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (status, result) = OwnedChild::spawn(driver).finish();
    assert!(status.success(), "{result}");
    assert_eq!(result["state"], "not_started");
    assert_eq!(result["target_started"], false);
    assert_eq!(result["scope_closed"], true);
    assert!(!fixture.root.join("payload").exists());
}

#[test]
fn resource_facts_are_from_the_actual_waited_payload_and_have_byte_units() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.executable,
        r"#!/usr/bin/env python3
import time
memory=bytearray(24*1024*1024)
for offset in range(0,len(memory),4096): memory[offset]=1
until=time.process_time()+.04
while time.process_time()<until: pass
print('measured payload')
",
    )
    .unwrap();
    let (status, result) = OwnedChild::spawn(fixture.command(3000)).finish();
    assert!(status.success(), "{result}");
    assert_eq!(result["state"], "completed");
    assert_eq!(result["exit_code"], 0);
    let usage = &result["payload_usage"];
    assert_eq!(usage["scope"], "waited_payload");
    assert!(usage["wall_millis"].as_u64().unwrap() >= 30);
    let cpu = usage["user_cpu_micros"].as_u64().unwrap() + usage["system_cpu_micros"].as_u64().unwrap();
    assert!(cpu >= 30_000, "{usage}");
    assert!(usage["max_rss_bytes"].as_u64().unwrap() >= 24 * 1024 * 1024, "{usage}");
}
