use crate::kernel::{Event, Kernel, Rejected, ResultId, ScopeBinding, SpawnFact, StageBinding, StageKey};
use lean_rs_worker_parent::{LeanWorkerNativeScope, LeanWorkerNativeScopeLease};
use lean_toolchain::native_guardian::{GuardianHandle, GuardianReadLoan, NativeProcessIdentity, NativeReadDomain};
use lean_toolchain::process_group::{OwnedCommandGroup, group_present};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const ROOT_ENV: &str = "LEAN_STAGE_RUNTIME_ROOT";
const POLL: Duration = Duration::from_millis(5);
const WAIT: Duration = Duration::from_secs(2);
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn wait(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < WAIT, "bounded dummy observation expired");
        thread::sleep(POLL);
    }
}
fn write_json(root: &Path, name: &str, value: &Value) {
    let pending = root.join(format!("{name}.pending"));
    fs::write(&pending, serde_json::to_vec(value).unwrap()).unwrap();
    fs::rename(pending, root.join(name)).unwrap();
}
fn read_json(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}
fn current_root() -> PathBuf {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap())
        .canonicalize()
        .unwrap();
    assert!(root.file_name().unwrap().to_string_lossy().starts_with("scope-"));
    assert!(
        root.parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("lean-stage-runtime-")
    );
    assert!(root.starts_with(std::env::temp_dir().canonicalize().unwrap()));
    root
}
fn fixture_command(root: &Path, name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env(ROOT_ENV, root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub(crate) struct Fixture {
    directory: Option<tempfile::TempDir>,
    pub(crate) root: PathBuf,
    pub(crate) project: PathBuf,
    started: Instant,
}
impl Fixture {
    pub(crate) fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("lean-stage-runtime-")
            .tempdir()
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        Self {
            directory: Some(directory),
            root,
            project,
            started: Instant::now(),
        }
    }
    // Borrow only test paths in another process; no TempDir deletion authority,
    // native Child, custody descriptor or private lifetime channel crosses here.
    #[allow(
        dead_code,
        reason = "shared fixture: only service-loss cases attach from another process"
    )]
    pub(crate) fn open_in_helper(root: &Path) -> Self {
        let root = root.canonicalize().unwrap();
        assert!(root.starts_with(std::env::temp_dir().canonicalize().unwrap()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("lean-stage-runtime-")
        );
        let project = root.join("project");
        assert!(project.is_dir());
        Self {
            directory: None,
            root,
            project,
            started: Instant::now(),
        }
    }
    pub(crate) fn leaf_starts(&self, key: StageKey) -> Vec<Value> {
        let text = fs::read_to_string(self.root.join("leaf-starts.jsonl")).unwrap_or_default();
        text.lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["stage"] == format!("{key:?}"))
            .collect()
    }
    pub(crate) fn root_starts(&self) -> usize {
        fs::read_to_string(self.root.join("root-starts.jsonl"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    pub(crate) fn assert_no_safety_exit(&self) {
        assert!(
            self.started.elapsed() < Duration::from_secs(3),
            "qualification must precede fixture safety expiry"
        );
        for entry in fs::read_dir(&self.root).unwrap() {
            let path = entry.unwrap().path();
            assert!(!path.join("test-safety-expired").exists());
            assert!(!path.join("guardian-safety-expired").exists());
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking()
            && let Some(directory) = self.directory.take()
        {
            eprintln!("preserved failed stage runtime fixture: {}", directory.keep().display());
        }
    }
}

// Raw spawn is guarded before any fallible identity/handshake work. Root cannot
// spawn even its guardian until actual Child adoption is acknowledged.
struct RawChild(Option<Child>);
impl Drop for RawChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut()
            && matches!(child.try_wait(), Ok(None))
        {
            let _kill = child.kill();
            let _wait = child.wait();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoFault {
    None,
    BeforeWrite,
    AfterWrite,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Dispatch {
    Sent,
    NotSent,
    Unconfirmed,
    AlreadyTaken,
    Stale,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservedKind {
    Spawn,
    Result,
    StageClosed,
    GenerationClosed,
}
#[derive(Clone)]
pub(crate) struct Observation {
    binding: StageBinding,
    scope: ScopeBinding,
    key: StageKey,
    kind: ObservedKind,
    result: Option<ResultId>,
    root_birth: NativeProcessIdentity,
}
impl Observation {
    pub(crate) fn kind(&self) -> ObservedKind {
        self.kind
    }
    pub(crate) fn deliver(&self, kernel: &mut Kernel) -> Result<(), Rejected> {
        let accepted = kernel.stage(self.key).ok_or(Rejected::UnknownStage)?;
        if accepted.binding != self.binding {
            return Err(Rejected::BindingMismatch);
        }
        match self.kind {
            ObservedKind::Spawn => {
                kernel.apply(self.binding, Event::SpawnObserved(SpawnFact::Spawned))?;
            }
            ObservedKind::Result => {
                kernel.apply(
                    self.binding,
                    Event::ResultObserved(self.result.ok_or(Rejected::ResultUnknown)?),
                )?;
            }
            ObservedKind::StageClosed => {
                kernel.apply(self.binding, Event::StageClosed)?;
            }
            ObservedKind::GenerationClosed => {
                if kernel.scope().binding == self.scope {
                    kernel.observe_generation_closed(self.scope)?;
                }
                // A genuine old full-scope observation says nothing about the
                // current generation. Keep the original identity, never retarget.
            }
        }
        Ok(())
    }
    pub(crate) fn root_birth(&self) -> &NativeProcessIdentity {
        &self.root_birth
    }
}

pub(crate) struct NativeStage {
    key: StageKey,
    binding: StageBinding,
    scope: ScopeBinding,
    root: PathBuf,
    birth: NativeProcessIdentity,
    group: OwnedCommandGroup,
    lease: Option<LeanWorkerNativeScopeLease>,
    go: Option<ChildStdin>,
    closed: Option<[Observation; 2]>,
}
impl NativeStage {
    pub(crate) fn key(&self) -> StageKey {
        self.key
    }
    pub(crate) fn binding(&self) -> StageBinding {
        self.binding
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn birth(&self) -> &NativeProcessIdentity {
        &self.birth
    }
    // Test setup asserts success. A failure never emits target SpawnFailed or a
    // closed observation; its already-installed cleanup owner retains the child.
    pub(crate) fn prepare(fixture: &Fixture, key: StageKey, binding: StageBinding, scope: ScopeBinding) -> Self {
        let root = fixture.root.join(format!(
            "scope-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let tag = format!("{key:?}");
        write_json(
            &root,
            "input.json",
            &json!({"project":fixture.project,"stage":tag,"base":fixture.root}),
        );
        let executable = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .replace('\'', "'\\''");
        fs::write(
            root.join("guardian"),
            format!("#!/bin/sh\nexec '{executable}' --exact fixtures::guardian_helper --nocapture\n"),
        )
        .unwrap();
        fs::set_permissions(root.join("guardian"), fs::Permissions::from_mode(0o700)).unwrap();
        let lifetime = LeanWorkerNativeScope::prepare(&fixture.root.join("owners"), WAIT, None).unwrap();
        let mut command = fixture_command(&root, "fixtures::root_helper");
        command
            .stdin(Stdio::piped())
            .stderr(File::create(root.join("root.stderr")).unwrap());
        lifetime.configure(&mut command);
        let mut raw = RawChild(Some(command.spawn().unwrap()));
        let pid = raw.0.as_ref().unwrap().id();
        let mut birth = None;
        wait(|| {
            birth = NativeProcessIdentity::capture(pid)
                .unwrap()
                .filter(|id| id.pid == id.session && id.pid == id.process_group);
            birth.is_some()
        });
        let birth = birth.unwrap();
        let group = match OwnedCommandGroup::adopt_private_session(raw.0.take().unwrap(), &birth) {
            Ok(group) => group,
            Err((error, child)) => {
                raw.0 = Some(child);
                panic!("could not adopt dummy child: {error}");
            }
        };
        let mut native = Self {
            key,
            binding,
            scope,
            root,
            birth,
            group,
            lease: None,
            go: None,
            closed: None,
        };
        native.go = native.group.take_stdin();
        assert!(native.go.is_some());
        fs::write(native.root.join("scope-adopted"), b"actual child owned").unwrap();
        native.lease = Some(lifetime.accept(pid, WAIT).unwrap());
        assert_eq!(native.lease.as_ref().unwrap().owner(), &native.birth);
        native.lease.as_mut().unwrap().acknowledge().unwrap();
        wait(|| native.root.join("prepared").exists());
        native
    }
    pub(crate) fn send_go(&mut self, fault: GoFault) -> Dispatch {
        let Some(mut go) = self.go.take() else {
            return Dispatch::AlreadyTaken;
        };
        if fault == GoFault::BeforeWrite {
            drop(go);
            return Dispatch::NotSent;
        }
        let sent = go.write_all(b"G").is_ok();
        drop(go);
        if !sent || fault == GoFault::AfterWrite {
            Dispatch::Unconfirmed
        } else {
            Dispatch::Sent
        }
    }
    pub(crate) fn request_stop(&mut self) -> io::Result<()> {
        drop(self.go.take()); // no pending launcher survives stopping
        self.group.request_stop()
    }
    pub(crate) fn heartbeat(&self) -> u64 {
        fs::metadata(self.root.join("heartbeat")).map_or(0, |m| m.len())
    }
    pub(crate) fn finish_target(&self) {
        fs::write(self.root.join("finish-target"), b"fixture completion").unwrap();
    }
    pub(crate) fn target_alive(&self) -> bool {
        read_json(&self.root.join("body-started.json"))
            .ok()
            .and_then(|row| serde_json::from_value::<NativeProcessIdentity>(row["target"].clone()).ok())
            .is_some_and(|target| target.is_live().unwrap())
    }
    fn observation(&self, kind: ObservedKind, result: Option<ResultId>) -> Observation {
        Observation {
            binding: self.binding,
            scope: self.scope,
            key: self.key,
            kind,
            result,
            root_birth: self.birth.clone(),
        }
    }
    pub(crate) fn observations(&self) -> io::Result<Vec<Observation>> {
        let mut out = Vec::new();
        if self.root.join("body-started.json").exists() {
            let started = read_json(&self.root.join("body-started.json"))?;
            if started["root_pid"] != self.birth.pid {
                return Err(io::Error::other("body source differs"));
            }
            out.push(self.observation(ObservedKind::Spawn, None));
        }
        if self.root.join("result-observed.json").exists() {
            let result = read_json(&self.root.join("result-observed.json"))?;
            let started = read_json(&self.root.join("body-started.json"))?;
            if result["target_pid"] != started["target"]["pid"]
                || result["exit_ok"] != true
                || result["root_pid"] != self.birth.pid
            {
                return Err(io::Error::other("dummy result/actual wait differs"));
            }
            let value = result["value"]
                .as_u64()
                .ok_or_else(|| io::Error::other("dummy value missing"))?;
            out.push(self.observation(ObservedKind::Result, Some(ResultId(u128::from(value)))));
        }
        Ok(out)
    }
    pub(crate) fn closed_observations(&mut self) -> io::Result<Vec<Observation>> {
        if let Some(closed) = &self.closed {
            return Ok(closed.to_vec());
        }
        // This is the SDK's complete owned-group observation, not direct-child
        // exit or a new census of an obsolete numeric PGID after reaping.
        if self.group.try_wait()?.is_none() {
            return Ok(Vec::new());
        }
        drop(self.go.take());
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| io::Error::other("guardian binding unavailable; closure uncertain"))?;
        if lease.guardian_alive()? {
            return Ok(Vec::new());
        }
        let closed = [
            self.observation(ObservedKind::StageClosed, None),
            self.observation(ObservedKind::GenerationClosed, None),
        ];
        self.closed = Some(closed.clone());
        Ok(closed.to_vec())
    }
}
impl Drop for NativeStage {
    fn drop(&mut self) {
        if self.closed.is_some() {
            return;
        }
        drop(self.go.take());
        let _stop = self.group.request_stop();
        let started = Instant::now();
        while started.elapsed() < WAIT {
            if self.closed_observations().is_ok_and(|events| !events.is_empty()) {
                return;
            }
            thread::sleep(POLL);
        }
        eprintln!("dummy cleanup/guardian closure uncertain: {}", self.root.display());
        assert!(
            thread::panicking(),
            "normal test exit cannot hide incomplete native cleanup"
        );
        // Failed setup remains a test failure; Drop never invents kernel closure.
    }
}

const TARGET: &str = r"
import os,pathlib,signal,sys,time,json
root=pathlib.Path(sys.argv[1]); base=root.parent
cfg=json.loads((root/'input.json').read_text())
signal.alarm(8)
for fd in range(3,4096):
 try: os.close(fd)
 except OSError: pass
row=json.dumps({'stage':cfg['stage'],'pid':os.getpid(),'root':str(root)})+'\n'
fd=os.open(base/'leaf-starts.jsonl',os.O_CREAT|os.O_APPEND|os.O_WRONLY,0o600);os.write(fd,row.encode());os.close(fd)
(root/'target-started').write_text(str(os.getpid()))
with (root/'heartbeat').open('ab',buffering=0) as out:
 while not (root/'finish-target').exists():
  out.write(b'x');time.sleep(.005)
(root/'target-result.pending').write_text(json.dumps({'pid':os.getpid(),'value':6*7}))
(root/'target-result.pending').replace(root/'target-result.json')
";

#[test]
fn guardian_helper() {
    if std::env::var_os(ROOT_ENV).is_none() {
        return;
    }
    let root = current_root();
    let worker = rustix::process::getppid().unwrap().as_raw_nonzero().get();
    let birth = NativeProcessIdentity::capture(std::process::id()).unwrap().unwrap();
    write_json(
        &root,
        "guardian-birth.json",
        &json!({"birth":birth,"parent_pid":worker}),
    );
    if root.parent().unwrap().join("service-death-case").exists() {
        // Failure-only dummy rescue. This cannot qualify closure: it may release
        // a loan before payload expiry. No recorded PID grants signal authority.
        let watched = root.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(8));
            let _marker = fs::write(watched.join("guardian-safety-expired"), b"not a pass");
            let _kill = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
        });
    }
    if let Err(error) = lean_toolchain::native_guardian::run_guardian_cli() {
        fs::write(root.join("guardian-error"), error.to_string()).unwrap();
        panic!("real guardian failed: {error}");
    }
    assert!(!group_present(u32::try_from(worker).unwrap()).unwrap());
    fs::write(root.join("guardian-returned"), b"real guardian drain returned").unwrap();
}
#[test]
fn root_helper() {
    if std::env::var_os(ROOT_ENV).is_none() {
        return;
    }
    let root = current_root();
    rustix::process::setsid().unwrap();
    let cfg = read_json(&root.join("input.json")).unwrap();
    let base = root.parent().unwrap();
    let mut audit = OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join("root-starts.jsonl"))
        .unwrap();
    writeln!(audit, "{}", json!({"pid":std::process::id(),"parent_pid":rustix::process::getppid().unwrap().as_raw_nonzero().get(),"root":root})).unwrap();
    drop(audit);
    wait(|| root.join("scope-adopted").exists());
    let mut guardian = GuardianHandle::spawn(
        &root.join("guardian"),
        &PathBuf::from(std::env::var_os("LEAN_RS_WORKER_PARENT_SOCKET").unwrap()),
        &std::env::var("LEAN_RS_WORKER_PARENT_NONCE").unwrap(),
        WAIT,
    )
    .unwrap();
    let project = PathBuf::from(cfg["project"].as_str().unwrap());
    assert_eq!(project, base.join("project"));
    let lock_path = lean_toolchain::shared_writer::project_lock_path(&project).unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock_shared().unwrap();
    let metadata = lock.metadata().unwrap();
    guardian
        .retain_reads(
            &[GuardianReadLoan {
                domain: NativeReadDomain {
                    project_root: project,
                    lock_path,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    epoch: "dummy-prebuilt-inputs".into(),
                },
                descriptor: lock.as_fd(),
            }],
            WAIT,
        )
        .unwrap();
    drop(lock); // Guardian is sole actual SH custodian after ACK.
    let watched = root.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(8));
        let _marker = fs::write(watched.join("test-safety-expired"), b"not a pass");
        let _kill = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    });
    fs::write(root.join("prepared"), b"actual loan ACK; waiting once for GO").unwrap();
    let mut byte = [0u8];
    if io::stdin().read_exact(&mut byte).is_err() {
        fs::write(root.join("receiver-no-go"), b"sender sealed at EOF").unwrap();
        return;
    }
    assert_eq!(byte, *b"G");
    let mut target = RawChild(Some(
        Command::new("python3")
            .args(["-c", TARGET])
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    ));
    wait(|| root.join("target-started").exists());
    let target_id = target.0.as_ref().unwrap().id();
    let identity = NativeProcessIdentity::capture(target_id).unwrap().unwrap();
    assert_eq!(identity.session, std::process::id());
    assert_eq!(identity.process_group, std::process::id());
    write_json(
        &root,
        "body-started.json",
        &json!({"target":identity,"root_pid":std::process::id()}),
    );
    let mut finished = false;
    loop {
        if !finished && let Some(status) = target.0.as_mut().unwrap().try_wait().unwrap() {
            if status.success() {
                let value = read_json(&root.join("target-result.json")).unwrap();
                assert_eq!(value["pid"], target_id);
                write_json(
                    &root,
                    "result-observed.json",
                    &json!({"target_pid":target_id,"root_pid":std::process::id(),"exit_ok":true,"value":value["value"]}),
                );
            }
            finished = true;
        }
        thread::sleep(POLL);
    }
}

pub(crate) struct Contender {
    child: RawChild,
    root: PathBuf,
}
impl Contender {
    pub(crate) fn start(native: &NativeStage) -> Self {
        Self::start_at(native.root(), native.birth())
    }
    // Metadata-only startup. The controller never opens a protective lock or
    // adopts the original worker Child merely to start an independent contender.
    pub(crate) fn start_at(root: &Path, birth: &NativeProcessIdentity) -> Self {
        let root = root.to_path_buf();
        write_json(&root, "contender.json", &json!({"worker":birth}));
        let child = RawChild(Some(
            fixture_command(&root, "fixtures::contender_helper")
                .stderr(File::create(root.join("contender.stderr")).unwrap())
                .spawn()
                .unwrap(),
        ));
        wait(|| root.join("contender-blocked").exists());
        Self { child, root }
    }
    pub(crate) fn finish(mut self) -> Value {
        fs::write(self.root.join("scope-closed"), b"actual native closure observed").unwrap();
        let mut status = None;
        wait(|| {
            status = self.child.0.as_mut().unwrap().try_wait().unwrap();
            status.is_some()
        });
        assert!(status.unwrap().success(), "{}", self.root.display());
        read_json(&self.root.join("contender-result.json")).unwrap()
    }
}
#[test]
fn contender_helper() {
    if std::env::var_os(ROOT_ENV).is_none() {
        return;
    }
    let root = current_root();
    let cfg = read_json(&root.join("input.json")).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lean_toolchain::shared_writer::project_lock_path(Path::new(cfg["project"].as_str().unwrap())).unwrap())
        .unwrap();
    assert!(matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    fs::write(root.join("contender-blocked"), b"actual EX attempt blocked").unwrap();
    wait(|| match lock.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(error) => panic!("lock failed: {error}"),
    });
    // These process observations FOLLOW the acquisition, not an atomic snapshot
    // at that instant. The heartbeat marker also has a scheduling gap.
    let worker: NativeProcessIdentity =
        serde_json::from_value(read_json(&root.join("contender.json")).unwrap()["worker"].clone()).unwrap();
    let present = group_present(worker.process_group).unwrap();
    let worker_live = worker.is_live().unwrap();
    let metadata = lock.metadata().unwrap();
    let managed_writer_ready = lean_toolchain::native_guardian::protected_native_reads_stopped(
        &root.parent().unwrap().join("owners"),
        &lean_toolchain::shared_writer::project_lock_path(Path::new(cfg["project"].as_str().unwrap())).unwrap(),
        (metadata.dev(), metadata.ino()),
    )
    .unwrap();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("heartbeat"))
        .unwrap()
        .write_all(b"|EX|")
        .unwrap();
    write_json(
        &root,
        "contender-acquired.json",
        &json!({"group_present":present,"worker_live":worker_live,"managed_writer_ready":managed_writer_ready}),
    );
    wait(|| root.join("scope-closed").exists());
    thread::sleep(Duration::from_millis(35));
    let text = fs::read_to_string(root.join("heartbeat")).unwrap();
    let after = text.split_once("|EX|").unwrap().1.len();
    write_json(
        &root,
        "contender-result.json",
        &json!({"group_present":present,"worker_live":worker_live,"managed_writer_ready":managed_writer_ready,"payload_after_marker":after}),
    );
    drop(lock); // Close-only. Never unlock or change a live lock mode.
}
