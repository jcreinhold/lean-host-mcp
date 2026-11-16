//! Every CLI client uses the same canonical project domain as the Rust SDK.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use lean_toolchain::shared_reader::{SharedReadProxy, SharedReader};
use lean_toolchain::shared_writer::{SharedWriter, project_lock_path};
use lean_toolchain::{SourcePackageError, SourcePackageManifestPolicy, SourcePackageMaterializationRequest};
use serde_json::{Value, json};
use serial_test::serial;

const LIMIT: Duration = Duration::from_secs(5);
const ADMISSION: &str = "0.05";

fn executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    domain: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("private project fixture");
        let root = directory
            .path()
            .canonicalize()
            .expect("canonical fixture")
            .join("Project");
        fs::create_dir_all(root.join(".lake")).expect("prepare empty build directory");
        let domain = project_lock_path(&root).expect("canonical project domain");
        Self {
            _directory: directory,
            root,
            domain,
        }
    }
}

struct Endpoints {
    _directory: tempfile::TempDir,
    socket: PathBuf,
    listener: UnixListener,
    fifo: PathBuf,
    _control: File,
}

impl Endpoints {
    fn new() -> Self {
        // Keep Darwin Unix-socket paths below sockaddr_un's short limit.
        let directory = tempfile::Builder::new()
            .prefix("domain-")
            .tempdir_in("/tmp")
            .expect("private endpoints");
        let root = directory.path().canonicalize().expect("canonical endpoint directory");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private endpoint permissions");
        let socket = root.join("fd.sock");
        let listener = UnixListener::bind(&socket).expect("reader transfer listener");
        listener.set_nonblocking(true).expect("nonblocking listener");
        let fifo = root.join("control.fifo");
        let made = Command::new("/usr/bin/mkfifo")
            .args(["-m", "600"])
            .arg(&fifo)
            .status()
            .expect("create private proxy FIFO");
        assert!(made.success(), "private proxy FIFO creation failed");
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .expect("keep proxy caller alive");
        Self {
            _directory: directory,
            socket,
            listener,
            fifo,
            _control: control,
        }
    }

    fn no_reader_ack(&self) {
        assert!(
            matches!(self.listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "a reader connected despite refused admission"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum Client {
    Reader,
    Proxy,
    Writer,
}

impl Client {
    fn command(self, root: &Path, domain: &Path, endpoints: &Endpoints, timeout: &str) -> Command {
        let mut command = Command::new(executable());
        let name = match self {
            Self::Reader => "reader",
            Self::Proxy => "read-proxy",
            Self::Writer => "writer",
        };
        command.arg(name).arg(root).arg(domain).arg(timeout);
        match self {
            Self::Reader => {
                command.arg(&endpoints.socket).arg("domain-test-nonce");
            }
            Self::Proxy => {
                command.arg("domain-test-nonce").arg(&endpoints.fifo);
            }
            Self::Writer => {}
        }
        command.env_remove("LEAN_HOST_MCP_WRITER_LOCK_LOANS");
        command
    }
}

struct OwnedHelper {
    child: Option<Child>,
    lines: Receiver<Vec<u8>>,
    observed: Vec<u8>,
}

impl OwnedHelper {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn owned CLI helper");
        let output = child.stdout.take().expect("helper stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = io::BufReader::new(output);
            loop {
                let mut line = Vec::new();
                let count = reader
                    .by_ref()
                    .take(1_048_577)
                    .read_until(b'\n', &mut line)
                    .expect("read helper frame");
                if count == 0 || sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child: Some(child),
            lines,
            observed: Vec::new(),
        }
    }

    fn send(&mut self, frame: &Value) {
        let mut bytes = serde_json::to_vec(frame).expect("encode bounded test command");
        bytes.push(b'\n');
        let input = self
            .child
            .as_mut()
            .expect("owned helper")
            .stdin
            .as_mut()
            .expect("helper stdin");
        match input.write_all(&bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
            Err(error) => panic!("write helper control frame: {error}"),
        }
    }

    fn frame(&mut self) -> Value {
        let line = self.lines.recv_timeout(LIMIT).expect("bounded helper reply");
        self.observed.extend_from_slice(&line);
        serde_json::from_slice(&line).expect("valid helper JSON")
    }

    fn close_input(&mut self) {
        drop(self.child.as_mut().expect("owned helper").stdin.take());
    }

    fn finish(mut self) -> Output {
        let deadline = Instant::now().checked_add(LIMIT).expect("bounded test deadline");
        let status: ExitStatus = loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("owned helper")
                .try_wait()
                .expect("observe owned helper")
            {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "helper did not exit within the test deadline"
            );
            thread::sleep(Duration::from_millis(5));
        };
        loop {
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => self.observed.extend_from_slice(&line),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("helper stdout remained open after exit"),
            }
        }
        let output = self
            .child
            .take()
            .expect("owned helper")
            .wait_with_output()
            .expect("reap owned helper");
        assert_eq!(output.status, status);
        Output {
            status,
            stdout: std::mem::take(&mut self.observed),
            stderr: output.stderr,
        }
    }
}

impl Drop for OwnedHelper {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            // This is the exact child created above. No discovered PID or group
            // is ever used as a cleanup target.
            if matches!(child.try_wait(), Ok(None)) {
                let _kill = child.kill();
            }
            let _wait = child.wait();
        }
    }
}

fn marker_command(marker: &Path) -> Value {
    json!({"action":"run", "command":["/bin/sh", "-c", "printf dispatched > \"$1\"", "domain-test", marker], "timeout":1.0})
}

fn refused(client: Client, fixture: &Fixture, domain: &Path, expect_domain_error: bool) {
    let endpoints = Endpoints::new();
    let marker = fixture.root.join("must-not-dispatch");
    let mut helper = OwnedHelper::spawn(&mut client.command(&fixture.root, domain, &endpoints, ADMISSION));
    if matches!(client, Client::Writer) {
        helper.send(&marker_command(&marker));
    }
    helper.close_input();
    let output = helper.finish();
    assert!(!output.status.success(), "{client:?} unexpectedly admitted: {output:?}");
    assert!(
        output.stdout.is_empty(),
        "{client:?} emitted an admission/GO receipt: {output:?}"
    );
    assert!(!marker.exists(), "{client:?} dispatched the queued command");
    if expect_domain_error {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("canonical project domain"),
            "{client:?} failed for a different reason: {output:?}"
        );
        assert!(!domain.exists(), "{client:?} created the alternate domain");
    }
    endpoints.no_reader_ack();
}

fn cli_writer_succeeds(fixture: &Fixture) {
    let endpoints = Endpoints::new();
    let marker = fixture.root.join("successful-dispatch");
    let mut helper = OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &endpoints, "2"));
    assert_eq!(helper.frame()["state"], "locked");
    helper.send(&marker_command(&marker));
    assert_eq!(helper.frame()["state"], "started");
    let stopped = helper.frame();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["owned_work_stopped"], true);
    assert_eq!(stopped["result"], "result");
    assert_eq!(stopped["exit_code"], 0);
    helper.close_input();
    assert!(helper.finish().status.success());
    assert_eq!(fs::read(&marker).expect("actual payload marker"), b"dispatched");
}

#[test]
#[serial]
fn alternate_cli_domains_are_rejected_before_ack_or_go_while_canonical_ex_is_held() {
    let fixture = Fixture::new();
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&fixture.domain)
        .expect("canonical writer guard");
    guard.lock().expect("hold canonical EX");
    for domain in [
        fixture.root.join(".lake/.stacks-orchestration.lock"),
        fixture.root.join("custom.lock"),
    ] {
        for client in [Client::Reader, Client::Proxy, Client::Writer] {
            refused(client, &fixture, &domain, true);
        }
    }
}

#[test]
#[serial]
fn sdk_writer_excludes_every_canonical_cli_client_and_drop_allows_progress() {
    let fixture = Fixture::new();
    let writer = SharedWriter::open(executable(), &fixture.root, LIMIT).expect("SDK writer admission");
    for client in [Client::Reader, Client::Proxy, Client::Writer] {
        refused(client, &fixture, &fixture.domain, false);
    }
    drop(writer);
    cli_writer_succeeds(&fixture);
}

#[test]
#[serial]
fn sdk_reader_and_proxy_exclude_the_canonical_cli_writer_until_release() {
    let fixture = Fixture::new();
    // All tests in this binary are serialized, and no other thread spawns a
    // process during this Darwin pre-body descriptor transfer.
    let reader = SharedReader::acquire(executable(), &fixture.root, LIMIT).expect("SDK reader custody");
    refused(Client::Writer, &fixture, &fixture.domain, false);
    drop(reader);
    let proxy = SharedReadProxy::acquire(executable(), &fixture.root, LIMIT).expect("SDK parent proxy custody");
    refused(Client::Writer, &fixture, &fixture.domain, false);
    drop(proxy);
    cli_writer_succeeds(&fixture);
}

#[test]
#[serial]
fn source_package_writer_keeps_the_canonical_primary_and_external_entry_loans() {
    let fixture = tempfile::tempdir().expect("source-package fixture");
    let base = fixture.path().canonicalize().expect("canonical source fixture");
    let source = base.join("source");
    fs::create_dir(&source).expect("source payload directory");
    for (name, bytes) in [
        (
            "lakefile.toml",
            "name = \"fixture_pkg\"\n[[lean_lib]]\nname = \"Fixture\"\n",
        ),
        ("lake-manifest.json", "{\"packages\": []}\n"),
        ("Fixture.lean", "def fixture : Nat := 0\n"),
    ] {
        fs::write(source.join(name), bytes).expect("write owned payload");
    }
    let input = SourcePackageMaterializationRequest {
        source_root: source,
        cache_root: base.join("cache"),
        package_name: "fixture_pkg".to_owned(),
        materialized_package_name: "fixture_pkg".to_owned(),
        library_name: "Fixture".to_owned(),
        source_digest: "fixture-digest".to_owned(),
        source_revision: "fixture-revision".to_owned(),
        crate_name: "fixture-crate".to_owned(),
        crate_version: "0.0.0".to_owned(),
        toolchain_label: "fixture".to_owned(),
        include_paths: ["lakefile.toml", "lake-manifest.json", "Fixture.lean"]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        generated_files: Vec::new(),
        sentinel_files: vec![PathBuf::from("Fixture.lean")],
        manifest_policy: SourcePackageManifestPolicy::ZeroPackages,
    };
    let package_root = lean_toolchain::with_materialized_source_package_with_writer(
        &input,
        executable(),
        LIMIT,
        || false,
        |package, writer| -> Result<PathBuf, SourcePackageError> {
            let domain = project_lock_path(&package.project_root).expect("stable generated-project domain");
            let entry = package
                .project_root
                .parent()
                .expect("cache digest directory")
                .join("fixture.lock");
            for path in [&domain, &entry] {
                let contender = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .expect("independent lock contender");
                assert!(
                    matches!(contender.try_lock(), Err(TryLockError::WouldBlock)),
                    "loan was not held: {path:?}"
                );
            }
            let marker = package.project_root.join("payload-marker");
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf owned > \"$1\"", "domain-test"])
                .arg(&marker);
            let output = writer.run(&command, LIMIT).map_err(|source| SourcePackageError::Io {
                action: "run owned test payload",
                path: marker.clone(),
                source,
            })?;
            assert!(output.status.success());
            assert_eq!(fs::read(&marker).expect("owned payload completed"), b"owned");
            Ok(package.project_root)
        },
    )
    .expect("one valid project-first/source-entry loan transaction");
    assert!(package_root.join("Fixture.lean").is_file());
}
