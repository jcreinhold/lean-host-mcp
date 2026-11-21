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
    // Explicit frozen images let the same real lifecycle assertions qualify a
    // release binary. An unset override retains ordinary Cargo integration tests.
    static HELPER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HELPER
        .get_or_init(|| match std::env::var_os("LEAN_HOST_MCP_CLI_TEST_HELPER") {
            Some(value) => {
                let path = PathBuf::from(value);
                assert!(
                    path.is_absolute() && path.is_file(),
                    "explicit helper must be an absolute executable file"
                );
                assert_eq!(path.canonicalize().expect("canonical test helper"), path);
                path
            }
            None => PathBuf::from(env!("CARGO_BIN_EXE_lean-host-mcp")),
        })
        .as_path()
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
    control: File,
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
            control,
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

    fn waiting_without_frame(&mut self) {
        assert!(
            self.child
                .as_mut()
                .expect("owned helper")
                .try_wait()
                .expect("live admission waiter")
                .is_none(),
            "helper exited before its held-domain wait"
        );
        match self.lines.recv_timeout(Duration::from_millis(50)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(frame) => panic!("helper emitted a frame beneath held writer custody: {frame:?}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("helper refused instead of waiting for writer custody"),
        }
        assert!(
            self.child
                .as_mut()
                .expect("owned helper")
                .try_wait()
                .expect("same live admission waiter")
                .is_none(),
            "helper exited rather than retaining the same admission attempt"
        );
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
#[ignore = "requires the matched standalone SDK writer with staged handoff support"]
fn embedded_writer_fences_extended_storage_charge_without_blocking_disjoint_work() {
    use std::os::unix::fs::MetadataExt as _;

    const REGISTRY_ENV: &str = lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV;
    let fixture = Fixture::new();
    let root = fixture.root.parent().expect("private fixture parent");
    let registry = root.join("owners");
    let scratch = root.join("scratch");
    let disjoint = root.join("Disjoint");
    for dir in [&registry, &scratch, &disjoint] {
        fs::create_dir(dir).expect("private fixture directory");
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("private fixture permissions");
    }
    let standalone = PathBuf::from(
        std::env::var_os("LEAN_HOST_MCP_MATCHED_SDK_WRITER").expect("set canonical matched standalone SDK writer"),
    );
    assert_eq!(standalone.canonicalize().expect("matched SDK writer"), standalone);

    let host = |project: &Path, lock: &Path| {
        let mut command = Command::new(executable());
        command
            .arg("writer")
            .arg(project)
            .arg(lock)
            .arg("2")
            .env(REGISTRY_ENV, &registry)
            .env("TMPDIR", &scratch)
            .env_remove("LEAN_HOST_MCP_WRITER_LOCK_LOANS");
        command
    };

    // Save a decodable predecessor for the reserve-before-owner-publication
    // cut. Otherwise the embedded writer's old record parser would refuse
    // the SDK phase fields before exercising the storage admission fence.
    let mut prior = OwnedHelper::spawn(&mut host(&fixture.root, &fixture.domain));
    assert_eq!(prior.frame()["state"], "locked");
    prior.close_input();
    assert!(prior.finish().status.success());
    let prior_owner = fs::read(&fixture.domain).expect("saved prior owner");

    let roots: Vec<_> = [
        ("project", &fixture.root),
        ("registry", &registry),
        ("scratch_parent", &scratch),
    ]
    .into_iter()
    .map(|(role, path)| {
        let metadata = fs::metadata(path).expect("exact storage root");
        json!({"role":role,"path":path,"device":metadata.dev(),"inode":metadata.ino()})
    })
    .collect();
    let digest = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    let request = json!({
        "version":1,"floor_bytes":0,"budget_bytes":1048576,"command_only":true,
        "handoff_sha256":digest,"roots":roots,
    });
    let mut command = Command::new(&standalone);
    command
        .arg("writer")
        .arg(&fixture.root)
        .arg(&fixture.domain)
        .args(["2", "--storage-request", &request.to_string()])
        .env(REGISTRY_ENV, &registry)
        .env_remove("LEAN_HOST_MCP_WRITER_LOCK_LOANS");
    let mut old = OwnedHelper::spawn(&mut command);
    let ready = old.frame();
    assert_eq!(ready["state"], "locked");
    assert_eq!(ready["handoff"]["handoff_sha256"], digest);
    let turn = ready["handoff"]["turn"].as_str().expect("extended storage turn");
    old.send(&json!({"action":"stage_handoff","turn":turn,"data_hex":"7b7d"}));
    assert_eq!(old.frame()["state"], "handoff_staged");
    old.close_input();
    assert!(
        !old.finish().status.success(),
        "EOF must retain the unclosed storage turn"
    );

    let asset = Path::new(ready["asset_dir"].as_str().expect("bound scratch"));
    let charge = registry.join(".writer-storage-v1").join(turn);
    let charged = fs::read(&charge).expect("real retained charge");
    let extended_owner = fs::read(&fixture.domain).expect("exact original owner");
    let registry_identity = fs::metadata(&registry).expect("exact registry");
    let registry_args = [
        "--registry",
        registry.to_str().expect("fixture path"),
        "--registry-device",
        &registry_identity.dev().to_string(),
        "--registry-inode",
        &registry_identity.ino().to_string(),
    ];
    let rollback = |confirmed: bool| {
        let mut check = Command::new(executable());
        check
            .args(["doctor", "writer-rollback"])
            .args(registry_args)
            .env(REGISTRY_ENV, &registry)
            .env_remove("LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER");
        if confirmed {
            check.arg("--confirm-launchers-drained");
        }
        check
    };
    let refused = rollback(true).output().expect("check unsettled rollback");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("writer storage custody remains open"),
        "{refused:?}"
    );
    assert_eq!(
        fs::read(&fixture.domain).expect("rollback leaves exact owner"),
        extended_owner
    );
    assert_eq!(fs::read(&charge).expect("rollback leaves charge"), charged);

    assert_eq!(
        serde_json::from_slice::<Value>(&charged).expect("valid charge")["handoff_sha256"],
        digest
    );
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.domain)
            .expect("same inode");
        file.try_lock().expect("all original process locks closed");
        file.set_len(0).expect("restore test-only predecessor");
        file.write_all(&prior_owner).expect("write predecessor bytes");
        file.sync_all().expect("sync fixture interruption cut");
    }

    let marker = root.join("dispatched");
    let overlap = OwnedHelper::spawn(&mut host(&fixture.root, &fixture.domain));
    let blocked = overlap.finish();
    assert!(!blocked.status.success() && blocked.stdout.is_empty(), "{blocked:?}");
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("prior writer storage custody remains open"),
        "{blocked:?}"
    );
    assert!(!marker.exists());
    assert_eq!(fs::read(&fixture.domain).expect("owner record retained"), prior_owner);
    assert_eq!(fs::read(&charge).expect("charge retained"), charged);
    assert_eq!(fs::read(asset.join("handoff.json")).expect("scratch retained"), b"{}");

    let domain = project_lock_path(&disjoint).expect("disjoint lock domain");
    let mut separate = OwnedHelper::spawn(&mut host(&disjoint, &domain));
    assert_eq!(separate.frame()["state"], "locked");
    separate.send(&marker_command(&marker));
    assert_eq!(separate.frame()["state"], "started");
    let stopped = separate.frame();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["owned_work_stopped"], true);
    assert_eq!(stopped["exit_code"], 0);
    separate.close_input();
    assert!(separate.finish().status.success());
    assert_eq!(fs::read(&marker).expect("disjoint payload ran"), b"dispatched");
    assert_eq!(fs::read(&charge).expect("disjoint admission retained charge"), charged);
    assert_eq!(
        fs::read(&fixture.domain).expect("unrelated owner unchanged"),
        prior_owner
    );
    // Restore the actual record to undo only the deliberate fixture cut.
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.domain)
            .expect("same inode");
        file.try_lock().expect("no overlapping work admitted");
        file.set_len(0).expect("restore actual test owner");
        file.write_all(&extended_owner).expect("restore exact owner bytes");
        file.sync_all().expect("sync fixture restoration");
    }
    let recovered = Command::new(&standalone)
        .args(["writer-storage", "recover"])
        .args(registry_args)
        .args(["--turn", turn, "--acknowledge-unknown-result"])
        .env(REGISTRY_ENV, &registry)
        .output()
        .expect("settle using the actual SDK recovery command");
    assert!(recovered.status.success(), "{recovered:?}");
    assert!(!charge.exists() && !asset.exists());
    assert!(!rollback(false).output().expect("unconfirmed drain").status.success());
    assert!(
        !rollback(true)
            .env("LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER", "{}")
            .output()
            .expect("active cutover")
            .status
            .success()
    );
    assert!(
        !rollback(true)
            .env(REGISTRY_ENV, &disjoint)
            .output()
            .expect("wrong registry")
            .status
            .success()
    );
    let allowed = rollback(true).output().expect("check legitimately settled rollback");
    assert!(allowed.status.success(), "{allowed:?}");
    let saved_registry = root.join("unavailable-owners");
    fs::rename(&registry, &saved_registry).expect("make disposable registry unavailable");
    let unavailable = rollback(true).output().expect("check unavailable registry");
    assert!(!unavailable.status.success());
    fs::rename(&saved_registry, &registry).expect("restore disposable registry");
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

fn wait_for_writer_demand(paths: &[PathBuf]) {
    let deadline = Instant::now().checked_add(LIMIT).expect("demand observation deadline");
    loop {
        if paths.iter().all(|path| {
            lean_toolchain::writer_demand::ReadAdmission::try_acquire(std::slice::from_ref(path))
                .expect("observe canonical domain demand")
                .is_none()
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "writer did not announce every domain before EX"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[serial]
fn waiting_cli_writer_fences_new_readers_and_caller_loss_clears_demand() {
    let fixture = Fixture::new();
    let original = SharedReadProxy::acquire(executable(), &fixture.root, LIMIT).expect("existing shared reader");
    let endpoints = Endpoints::new();
    let mut writer = OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &endpoints, "4"));
    wait_for_writer_demand(std::slice::from_ref(&fixture.domain));
    assert!(
        matches!(writer.lines.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "writer admitted beneath live SH"
    );
    let started = Instant::now();
    let error = SharedReadProxy::acquire(executable(), &fixture.root, LIMIT)
        .err()
        .expect("new reader must not overtake waiting writer");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "reader waited behind demand: {error}"
    );
    writer.close_input();
    let output = writer.finish();
    assert!(
        !output.status.success() && output.stdout.is_empty(),
        "cancelled writer released no stage: {output:?}"
    );
    assert!(
        lean_toolchain::writer_demand::ReadAdmission::try_acquire(std::slice::from_ref(&fixture.domain))
            .expect("closed caller leaves no demand")
            .is_some()
    );
    let successor =
        SharedReadProxy::acquire(executable(), &fixture.root, LIMIT).expect("compatible reader after demand closure");
    drop(successor);
    drop(original);
    cli_writer_succeeds(&fixture);
}

#[test]
#[serial]
fn partial_multi_domain_reader_refuses_demand_without_a_reader_writer_cycle() {
    let fixture = Fixture::new();
    let dependency = fixture.root.parent().expect("fixture parent").join("Dependency");
    fs::create_dir_all(dependency.join(".lake")).expect("dependency directory");
    fs::write(dependency.join("lake-manifest.json"), "{\"packages\":[]}").expect("dependency manifest");
    fs::write(
        fixture.root.join("lake-manifest.json"),
        serde_json::to_vec(&json!({
            "version":"1.3.0",
            "packages":[{"name":"dependency","type":"path","dir":dependency,"manifestFile":"lake-manifest.json"}]
        }))
        .expect("primary graph"),
    )
    .expect("primary manifest");
    let partial = SharedReadProxy::acquire(executable(), &fixture.root, LIMIT).expect("reader already holds first SH");
    let endpoints = Endpoints::new();
    let mut writer = OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &endpoints, "4"));
    let dependency_lock = project_lock_path(&dependency).expect("dependency lock domain");
    wait_for_writer_demand(&[fixture.domain.clone(), dependency_lock]);
    let started = Instant::now();
    assert!(SharedReadProxy::acquire(executable(), &dependency, LIMIT).is_err());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "partial reader waited while holding earlier SH"
    );
    drop(partial);
    assert_eq!(
        writer.frame()["state"],
        "locked",
        "writer succeeds after partial scope releases"
    );
    writer.close_input();
    assert!(writer.finish().status.success());
    cli_writer_succeeds(&fixture);
}

fn single_domain_proxy(fixture: &Fixture, endpoints: &Endpoints, admission: &str) -> OwnedHelper {
    let mut command = Client::Proxy.command(&fixture.root, &fixture.domain, endpoints, admission);
    command.arg("--single-domain-wait");
    OwnedHelper::spawn(&mut command)
}

fn one_proxy_ready(proxy: &mut OwnedHelper, fixture: &Fixture) {
    let ready = proxy.frame();
    assert_eq!(ready["state"], "read_ready");
    assert_eq!(
        ready["project_root"],
        fixture.root.to_str().expect("canonical UTF8 root")
    );
    assert_eq!(ready["nonce"], "domain-test-nonce");
    assert_eq!(ready["custody"], "proxy");
    assert!(ready["epoch"].as_str().is_some_and(|epoch| !epoch.is_empty()));
}

fn closed_proxy_with_one_ready(proxy: OwnedHelper) {
    let output = proxy.finish();
    assert!(
        output.status.success(),
        "proxy failed after actual read admission: {output:?}"
    );
    let frames = String::from_utf8(output.stdout).expect("UTF8 proxy frames");
    assert_eq!(
        frames.lines().count(),
        1,
        "same proxy emitted more than one ready frame"
    );
    assert_eq!(
        serde_json::from_str::<Value>(frames.trim()).expect("one ready frame")["state"],
        "read_ready"
    );
}

#[test]
#[serial]
fn explicit_single_domain_proxy_waits_through_demand_and_ex_in_one_attempt() {
    let fixture = Fixture::new();
    // Model a separate reader client with its own helper-owned SH. The metadata
    // proxy receives no artifact descriptor or partial multi-domain read scope.
    let original_endpoints = Endpoints::new();
    let mut original =
        OwnedHelper::spawn(&mut Client::Proxy.command(&fixture.root, &fixture.domain, &original_endpoints, "3"));
    one_proxy_ready(&mut original, &fixture);
    let writer_endpoints = Endpoints::new();
    let mut writer =
        OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &writer_endpoints, "4"));
    wait_for_writer_demand(std::slice::from_ref(&fixture.domain));
    let endpoints = Endpoints::new();
    let mut proxy = single_domain_proxy(&fixture, &endpoints, "4");
    let attempt_pid = proxy.child.as_ref().expect("actual proxy attempt").id();
    proxy.waiting_without_frame();

    drop(original_endpoints.control);
    assert!(original.finish().status.success());
    assert_eq!(
        writer.frame()["state"],
        "locked",
        "waiting proxy must not retain a gate that blocks writer EX"
    );
    proxy.waiting_without_frame();
    let marker = fixture.root.join("completed-writer-metadata");
    writer.send(&marker_command(&marker));
    assert_eq!(writer.frame()["state"], "started");
    let stopped = writer.frame();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["owned_work_stopped"], true);
    assert_eq!(stopped["result"], "result");
    assert_eq!(stopped["intervened"], false);
    assert_eq!(stopped["exit_code"], 0);
    writer.close_input();

    one_proxy_ready(&mut proxy, &fixture);
    assert_eq!(proxy.child.as_ref().expect("same proxy attempt").id(), attempt_pid);
    assert_eq!(
        fs::read(&marker).expect("metadata body only after ready"),
        b"dispatched"
    );
    assert!(writer.finish().status.success());
    drop(endpoints.control);
    closed_proxy_with_one_ready(proxy);
}

#[test]
#[serial]
fn explicit_single_domain_proxy_fifo_eof_while_waiting_emits_no_ready() {
    let fixture = Fixture::new();
    let writer_endpoints = Endpoints::new();
    let mut writer =
        OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &writer_endpoints, "3"));
    assert_eq!(writer.frame()["state"], "locked");
    let endpoints = Endpoints::new();
    let mut proxy = single_domain_proxy(&fixture, &endpoints, "3");
    proxy.waiting_without_frame();
    let started = Instant::now();
    drop(endpoints.control);
    let output = proxy.finish();
    assert!(
        !output.status.success(),
        "closed caller must refuse before ready: {output:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "caller loss must not publish readiness: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("caller closed"),
        "caller loss must remain explicit: {output:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "FIFO loss exceeded the bounded wait"
    );
    assert!(
        writer
            .child
            .as_mut()
            .expect("actual writer")
            .try_wait()
            .expect("writer still holds EX")
            .is_none()
    );
    writer.close_input();
    assert!(writer.finish().status.success());
}

#[test]
#[serial]
fn explicit_single_domain_proxy_uses_one_absolute_admission_deadline() {
    let fixture = Fixture::new();
    let writer_endpoints = Endpoints::new();
    let mut writer =
        OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &writer_endpoints, "3"));
    assert_eq!(writer.frame()["state"], "locked");
    let endpoints = Endpoints::new();
    let started = Instant::now();
    let proxy = single_domain_proxy(&fixture, &endpoints, "0.15");
    let output = proxy.finish();
    let elapsed = started.elapsed();
    assert!(
        !output.status.success() && output.stdout.is_empty(),
        "deadline must refuse without readiness: {output:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(100),
        "single-domain proxy refused immediately instead of waiting: {output:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "admission loop reset its original deadline"
    );
    let detail = String::from_utf8_lossy(&output.stderr);
    assert!(
        detail.contains("timed") || detail.contains("busy") || detail.contains("deadline"),
        "deadline detail missing: {output:?}"
    );
    assert!(
        writer
            .child
            .as_mut()
            .expect("actual writer")
            .try_wait()
            .expect("writer stayed live throughout budget")
            .is_none()
    );
    writer.close_input();
    assert!(writer.finish().status.success());
}

#[test]
#[serial]
fn own_completed_writer_can_transition_directly_to_one_metadata_proxy() {
    let fixture = Fixture::new();
    let writer_endpoints = Endpoints::new();
    let mut writer =
        OwnedHelper::spawn(&mut Client::Writer.command(&fixture.root, &fixture.domain, &writer_endpoints, "3"));
    assert_eq!(writer.frame()["state"], "locked");
    let marker = fixture.root.join("own-writer-completed");
    writer.send(&marker_command(&marker));
    assert_eq!(writer.frame()["state"], "started");
    let stopped = writer.frame();
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["owned_work_stopped"], true);
    assert_eq!(stopped["result"], "result");
    assert_eq!(stopped["intervened"], false);
    assert_eq!(stopped["exit_code"], 0);
    writer.close_input();
    // Start the read immediately after closing the writer control, before any
    // explicit writer wait or separate drain action.
    let endpoints = Endpoints::new();
    let mut proxy = single_domain_proxy(&fixture, &endpoints, "3");
    one_proxy_ready(&mut proxy, &fixture);
    assert_eq!(fs::read(&marker).expect("completed writer metadata"), b"dispatched");
    assert!(writer.finish().status.success());
    drop(endpoints.control);
    closed_proxy_with_one_ready(proxy);
}

#[test]
#[serial]
fn only_opted_in_pre_custody_contention_reports_normal_75() {
    let fixture = Fixture::new();
    let demand =
        lean_toolchain::writer_demand::WriterDemand::announce(std::slice::from_ref(&fixture.domain), LIMIT, &|| false)
            .expect("hold actual writer demand");
    for client in [Client::Reader, Client::Proxy] {
        let endpoints = Endpoints::new();
        let mut command = client.command(&fixture.root, &fixture.domain, &endpoints, "2");
        command.arg("--report-contention");
        let mut helper = OwnedHelper::spawn(&mut command);
        helper.close_input();
        let output = helper.finish();
        assert_eq!(
            output.status.code(),
            Some(75),
            "only known pre-custody contention: {output:?}"
        );
        assert!(output.stdout.is_empty(), "contention must not emit a Ready/frame");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("writer_demand"),
            "bounded reason diagnostic"
        );
        endpoints.no_reader_ack();
        let mut legacy = OwnedHelper::spawn(&mut client.command(&fixture.root, &fixture.domain, &endpoints, "2"));
        legacy.close_input();
        let output = legacy.finish();
        assert_eq!(
            output.status.code(),
            Some(1),
            "legacy fail-fast status unchanged: {output:?}"
        );
        assert!(output.stdout.is_empty());
        endpoints.no_reader_ack();
    }
    drop(demand);
}

#[test]
#[serial]
fn opted_in_hard_domain_errors_never_report_retryable_contention() {
    let fixture = Fixture::new();
    for client in [Client::Reader, Client::Proxy] {
        let endpoints = Endpoints::new();
        let bad = fixture.root.join("alternate.lock");
        let mut command = client.command(&fixture.root, &bad, &endpoints, "2");
        command.arg("--report-contention");
        let mut helper = OwnedHelper::spawn(&mut command);
        helper.close_input();
        let output = helper.finish();
        assert_eq!(
            output.status.code(),
            Some(1),
            "hard errors must not ask union retry: {output:?}"
        );
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("canonical project domain"));
        assert!(!bad.exists());
        endpoints.no_reader_ack();
    }
}
