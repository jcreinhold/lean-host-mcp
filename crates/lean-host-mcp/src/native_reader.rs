//! Guardian-owned execution for prebuilt external native readers.
//!
//! This command provides read custody and process closure, never a loaded Lean
//! ABI assertion or a semantic proof verdict. It launches one payload at most.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerNativePendingCleanup, LeanWorkerNativeScope, LeanWorkerNativeScopeLease, LeanWorkerNativeSpawnError,
};
use lean_toolchain::native_guardian::{GuardianHandle, NativeCapacitySlot, NativeProcessIdentity, NativeReadDomain};
use lean_toolchain::process_group::{MeasuredChild, WaitedChildUsage};
use lean_toolchain::shared_reader::SharedReader;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use serde::{Deserialize, Serialize};

mod control;
mod input;

pub const PROTOCOL: u32 = 1;
const POLL: Duration = Duration::from_millis(10);
const CLEANUP: Duration = Duration::from_secs(2);
const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 1024 * 1024;
const STDIN_ENV: &str = "LEAN_HOST_MCP_NATIVE_READER_STDIN";
const PROGRAM_CWD_ENV: &str = "LEAN_HOST_MCP_NATIVE_READER_CWD";
const PROGRAM_ROLE_ENV: &str = "LEAN_HOST_MCP_NATIVE_READER_ROLE";
const PRIVATE_ENV: &[&str] = &[
    STDIN_ENV,
    PROGRAM_CWD_ENV,
    PROGRAM_ROLE_ENV,
    "LEAN_RS_WORKER_PARENT_SOCKET",
    "LEAN_RS_WORKER_PARENT_NONCE",
    "LEAN_RS_WORKER_GUARDIAN_EXECUTABLE",
    "LEAN_RS_NATIVE_GUARDIAN_CONTROL_FD",
    "LEAN_RS_NATIVE_GUARDIAN_WORKER_PID",
    "LEAN_RS_WORKER_SHARED_ARTIFACT_READS",
    "LEAN_RS_PREPARED_HOST_ARTIFACTS",
    "LEAN_HOST_MCP_WRITER_LOCK_LOANS",
    "LEAN_HOST_MCP_WRITER_WORKSPACES",
];

/// Internal external-reader contract; all execution arguments follow `--`.
#[derive(Clone, Debug, clap::Args)]
pub struct NativeReaderArgs {
    #[arg(long)]
    project_root: PathBuf,
    /// Already selected physical read domain; its manifest is not re-expanded.
    #[arg(long)]
    artifact_root: Vec<PathBuf>,
    #[arg(long)]
    timeout_millis: u64,
    /// The actual configured MCP worker limit; inconsistent live limits refuse.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=128))]
    capacity_limit: u16,
    /// Stage one bounded stdin stream before native admission; closed by default.
    #[arg(long)]
    stdin: bool,
    /// Private already-nonblocking read pipe; any byte or EOF requests cancellation.
    #[arg(long, value_parser = clap::value_parser!(i32).range(3..))]
    cancel_fd: Option<i32>,
    /// Require complete payload stdout, allowing a clipped stderr diagnostic.
    #[arg(long)]
    stdout_result_frame: bool,
    #[arg(long, default_value_t = OUTPUT_LIMIT)]
    max_output_bytes: usize,
    #[arg(last = true, required = true)]
    command: Vec<OsString>,
}

/// One utility program whose inputs are prepared in an owner-held directory.
pub(crate) struct NativeReaderProgram {
    pub(crate) command: Vec<OsString>,
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) cwd: PathBuf,
    /// Frozen physical domains; only `project_root` is expanded as a workspace.
    pub(crate) artifact_roots: Vec<PathBuf>,
}

/// Run one private-input utility through the same native-reader scope.
/// The blocking owner must outlive its async waiter; that waiter signals the
/// supplied cancellation token on drop. The preparer runs before child spawn.
pub(crate) fn run_program(
    project_root: &Path,
    timeout: Duration,
    capacity_limit: usize,
    cancelled: &(impl Fn() -> bool + ?Sized),
    keepalive: Option<Box<dyn Send>>,
    prepare: impl FnOnce(&Path) -> io::Result<NativeReaderProgram>,
) -> io::Result<NativeReaderResult> {
    let helper = match crate::shared_writer::contract() {
        Ok(helper) => helper,
        Err(error) => return Ok(not_started(error)),
    };
    run_program_with_contract(
        &helper,
        project_root,
        timeout,
        capacity_limit,
        cancelled,
        keepalive,
        prepare,
    )
}

/// Explicit helper/registry seam for owned private fixtures and configured hosts.
pub(crate) fn run_program_with_contract(
    helper: &crate::shared_writer::WriterContract,
    project_root: &Path,
    timeout: Duration,
    capacity_limit: usize,
    cancelled: &(impl Fn() -> bool + ?Sized),
    keepalive: Option<Box<dyn Send>>,
    prepare: impl FnOnce(&Path) -> io::Result<NativeReaderProgram>,
) -> io::Result<NativeReaderResult> {
    let mut normalized_helper = helper.clone();
    normalized_helper.executable = match helper.executable.canonicalize() {
        Ok(executable) => executable,
        Err(error) => return Ok(not_started(error)),
    };
    let helper = &normalized_helper;
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let deadline = until(timeout)?;
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "native utility cancelled before preparation",
            ));
        }
        if !(1..=128).contains(&capacity_limit) {
            return Err(invalid("native utility capacity limit invalid"));
        }
        canonical(project_root)?;
        canonical(&helper.executable)?;
        if helper.protocol != crate::shared_writer::PROTOCOL {
            return Err(invalid("native utility helper protocol differs"));
        }
        let scope = reserve_scope(helper, capacity_limit, true, deadline, cancelled)?;
        // These files belong to the existing generation record, including after
        // controller death. Preparing Drop and closed-generation GC remove them.
        let input_root = scope.input_directory()?;
        let program = prepare(&input_root)?;
        let args = NativeReaderArgs {
            project_root: project_root.to_path_buf(),
            artifact_root: program.artifact_roots.clone(),
            timeout_millis: u64::try_from(remaining(deadline)?.as_millis()).map_err(io::Error::other)?,
            capacity_limit: u16::try_from(capacity_limit).map_err(io::Error::other)?,
            stdin: false,
            cancel_fd: None,
            stdout_result_frame: false,
            max_output_bytes: OUTPUT_LIMIT,
            command: program.command.clone(),
        };
        Ok::<_, io::Error>(((scope, deadline), program, args))
    }));
    let (scope, program, args) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => return Ok(not_started(error)),
        Err(_) => return Ok(not_started("utility owner panicked before bootstrap spawn")),
    };
    let mut spawned = false;
    let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_internal(
            &args,
            Some(&program),
            Some(helper),
            Some(scope),
            keepalive,
            cancelled,
            &mut spawned,
        )
    }));
    match execution {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if !spawned => Ok(not_started(error)),
        Ok(Err(error)) => Err(error),
        Err(_) if !spawned => Ok(not_started("utility owner panicked before bootstrap spawn")),
        Err(_) => Ok(result(
            None,
            false,
            Some("native utility owner panicked during or after bootstrap spawn; closure unconfirmed".into()),
        )),
    }
}

fn startup_failure(source: &io::Error, scope_closed: bool) -> NativeReaderResult {
    result(
        Some(PayloadResult {
            protocol: PROTOCOL,
            target_started: false,
            ..PayloadResult::default()
        }),
        scope_closed,
        Some(format!("native startup refused before payload ACK: {source}")),
    )
}

fn not_started(message: impl std::fmt::Display) -> NativeReaderResult {
    result(
        Some(PayloadResult {
            protocol: PROTOCOL,
            target_started: false,
            detail: Some(format!("native utility refused before bootstrap spawn: {message}")),
            ..PayloadResult::default()
        }),
        true,
        None,
    )
}

#[derive(Debug, Serialize)]
pub struct NativeReaderResult {
    pub protocol: u32,
    pub state: String,
    pub target_started: Option<bool>,
    pub exit_code: Option<i32>,
    pub exit_status: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub intervened: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_usage: Option<PayloadUsage>,
    pub scope_closed: bool,
    pub protected_domains: Vec<NativeReadDomain>,
    pub runtime_compatibility: serde_json::Value,
    pub detail: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PayloadResult {
    protocol: u32,
    target_started: bool,
    exit_code: Option<i32>,
    exit_status: Option<String>,
    stdout: String,
    stderr: String,
    output_truncated: bool,
    intervened: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdin_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdin_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload_usage: Option<PayloadUsage>,
    protected_domains: Vec<NativeReadDomain>,
    detail: Option<String>,
}

/// Facts from the sole kernel wait for the actual payload, not group totals.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadUsage {
    pub scope: String,
    pub wall_millis: u64,
    pub user_cpu_micros: u64,
    pub system_cpu_micros: u64,
    pub max_rss_bytes: u64,
}
impl PayloadUsage {
    fn from_waited(usage: &WaitedChildUsage) -> Option<Self> {
        Some(Self {
            scope: "waited_payload".into(),
            wall_millis: usage.wall_millis,
            user_cpu_micros: usage.user_cpu_micros,
            system_cpu_micros: usage.system_cpu_micros,
            max_rss_bytes: usage.max_rss_bytes?,
        })
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn until(timeout: Duration) -> io::Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .filter(|_| !timeout.is_zero())
        .ok_or_else(|| invalid("positive bounded native-reader timeout required"))
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let budget = deadline.saturating_duration_since(Instant::now());
    if budget.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "native-reader deadline elapsed",
        ))
    } else {
        Ok(budget)
    }
}

fn canonical(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.canonicalize()?.as_os_str() != path.as_os_str() {
        return Err(invalid("explicit canonical absolute native-reader path required"));
    }
    Ok(())
}

impl NativeReaderArgs {
    fn validate(&self) -> io::Result<()> {
        canonical(&self.project_root)?;
        if self.artifact_root.len() > lean_toolchain::native_guardian::MAX_READ_DOMAINS {
            return Err(invalid("native-reader explicit artifact-domain count exceeds limit"));
        }
        for root in &self.artifact_root {
            canonical(root)?;
        }
        let executable = self
            .command
            .first()
            .ok_or_else(|| invalid("payload executable required after --"))?;
        canonical(Path::new(executable))?;
        if !Path::new(executable).is_file() || !(1..=OUTPUT_LIMIT).contains(&self.max_output_bytes) {
            return Err(invalid("native-reader executable or output limit invalid"));
        }
        until(Duration::from_millis(self.timeout_millis))?;
        Ok(())
    }

    fn roots(&self) -> io::Result<Vec<PathBuf>> {
        let roots = lean_toolchain::shared_writer::project_artifact_roots_with_domains(
            std::slice::from_ref(&self.project_root),
            &self.artifact_root,
        )?;
        let mut domains = roots
            .into_iter()
            .map(|root| Ok((lean_toolchain::shared_writer::project_lock_path(&root)?, root)))
            .collect::<io::Result<Vec<_>>>()?;
        domains.sort_unstable();
        Ok(domains.into_iter().map(|(_, root)| root).collect())
    }

    fn append(&self, command: &mut Command) {
        command
            .arg("--project-root")
            .arg(&self.project_root)
            .arg("--timeout-millis")
            .arg(self.timeout_millis.to_string())
            .arg("--capacity-limit")
            .arg(self.capacity_limit.to_string())
            .arg("--max-output-bytes")
            .arg(self.max_output_bytes.to_string());
        for root in &self.artifact_root {
            command.arg("--artifact-root").arg(root);
        }
        if self.stdout_result_frame {
            command.arg("--stdout-result-frame");
        }
        if self.stdin {
            command.arg("--stdin");
        }
        command.arg("--").args(&self.command);
    }
}

enum CleanupJob {
    Measured(MeasuredChild),
    Pending {
        pending: LeanWorkerNativePendingCleanup,
        closed: std::sync::mpsc::Sender<()>,
        keepalive: Option<Box<dyn Send>>,
    },
    Native {
        lease: Box<LeanWorkerNativeScopeLease>,
        keepalive: Option<Box<dyn Send>>,
    },
}

fn finish_cleanup(job: CleanupJob) {
    match job {
        CleanupJob::Measured(mut child) => loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(POLL),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) => break,
                Err(_) => thread::sleep(POLL),
            }
        },
        CleanupJob::Pending {
            mut pending,
            keepalive,
            closed,
        } => loop {
            let _status = pending.try_wait();
            if matches!(pending.custody_closed(), Ok(true)) {
                drop(keepalive);
                let _observed = closed.send(());
                break;
            }
            thread::sleep(POLL);
        },
        CleanupJob::Native { mut lease, keepalive } => loop {
            let _stop = lease.kill();
            let _status = lease.try_wait();
            if matches!(lease.custody_closed(), Ok(true)) {
                let _inputs = lease.cleanup_inputs();
                drop(keepalive);
                break;
            }
            thread::sleep(POLL);
        },
    }
}

/// One scope owner, transferred to its pre-created reaper on every early return.
struct NativeGroupOwner {
    lease: Option<LeanWorkerNativeScopeLease>,
    keepalive: Option<Box<dyn Send>>,
    reaper: std::sync::mpsc::SyncSender<CleanupJob>,
    closed: bool,
}
impl NativeGroupOwner {
    fn lease(&mut self) -> io::Result<&mut LeanWorkerNativeScopeLease> {
        self.lease
            .as_mut()
            .ok_or_else(|| invalid("native lifetime ownership missing"))
    }
}
impl Drop for NativeGroupOwner {
    fn drop(&mut self) {
        if !self.closed
            && let Some(lease) = self.lease.take()
        {
            // This is the first and only job for the pre-created receiver.
            // The same actual Child and its resource lease remain owned.
            let _handoff = self.reaper.send(CleanupJob::Native {
                lease: Box::new(lease),
                keepalive: self.keepalive.take(),
            });
        }
    }
}

/// Allocate the handoff before starting an operation that can create a child.
fn cleanup_reaper() -> io::Result<std::sync::mpsc::SyncSender<CleanupJob>> {
    let (reaper, receiver) = std::sync::mpsc::sync_channel::<CleanupJob>(1);
    thread::Builder::new()
        .name("native-reader-reaper".into())
        .spawn(move || {
            if let Ok(job) = receiver.recv() {
                finish_cleanup(job);
            }
        })?;
    Ok(reaper)
}

struct ChildOwner {
    child: Option<MeasuredChild>,
    reaper: std::sync::mpsc::SyncSender<CleanupJob>,
}
impl ChildOwner {
    fn spawn_measured(command: Command) -> io::Result<Self> {
        let reaper = cleanup_reaper()?;
        Ok(Self {
            child: Some(MeasuredChild::spawn(command)?),
            reaper,
        })
    }
    fn get(&mut self) -> io::Result<&mut MeasuredChild> {
        self.child
            .as_mut()
            .ok_or_else(|| invalid("native-reader actual child missing"))
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    // Only a confirmed still-owned unreaped Child is signalled.
                    // ECHILD and every failed wait revoke that authority.
                    let _kill = child.kill();
                }
                Err(_) => {
                    let _handoff = self.reaper.send(CleanupJob::Measured(child));
                    return;
                }
            }
            let deadline = Instant::now()
                .checked_add(Duration::from_millis(100))
                .unwrap_or_else(Instant::now);
            if matches!(wait_child(&mut child, deadline), Ok(Some(_))) {
                return;
            }
            let _handoff = self.reaper.send(CleanupJob::Measured(child));
        }
    }
}

struct Capture<R> {
    reader: R,
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}
impl<R: Read + AsFd> Capture<R> {
    fn new(reader: R, limit: usize) -> io::Result<Self> {
        let flags = fcntl_getfl(&reader)?;
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK)?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            limit,
            truncated: false,
        })
    }
    fn poll(&mut self) -> io::Result<bool> {
        let mut buffer = [0_u8; 8192];
        // Bound each turn even when a payload writes continuously.
        for _ in 0..128 {
            match self.reader.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    let accepted = count.min(self.limit.saturating_sub(self.bytes.len()));
                    self.bytes
                        .extend_from_slice(buffer.get(..accepted).ok_or_else(|| invalid("capture count"))?);
                    self.truncated |= accepted != count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn result(payload: Option<PayloadResult>, scope_closed: bool, interrupted: Option<String>) -> NativeReaderResult {
    let target_started = payload.as_ref().map(|payload| payload.target_started);
    let completed = payload
        .as_ref()
        .is_some_and(|payload| payload.target_started && payload.exit_status.is_some());
    let state = if !scope_closed {
        "unknown"
    } else if interrupted.is_some()
        || payload
            .as_ref()
            .is_some_and(|payload| payload.intervened || payload.output_truncated)
    {
        "stopped"
    } else if completed {
        "completed"
    } else if target_started == Some(false) {
        "not_started"
    } else {
        "stopped"
    };
    let payload = payload.unwrap_or_default();
    NativeReaderResult {
        protocol: PROTOCOL,
        state: state.into(),
        target_started,
        exit_code: payload.exit_code,
        exit_status: payload.exit_status,
        stdout: payload.stdout,
        stderr: payload.stderr,
        output_truncated: payload.output_truncated,
        intervened: payload.intervened,
        stdin_sha256: payload.stdin_sha256,
        stdin_bytes: payload.stdin_bytes,
        payload_usage: payload.payload_usage,
        scope_closed,
        protected_domains: payload.protected_domains,
        runtime_compatibility: serde_json::json!({"status":"not_observed", "reason":"external_command"}),
        detail: interrupted.or(payload.detail),
    }
}

/// Run one external reader and emit one bounded lifecycle result.
///
/// # Errors
/// Returns pre-admission validation, pipe or serialization failures. Missing
/// output is never permission to replay a payload.
pub fn run_cli(args: &NativeReaderArgs) -> io::Result<bool> {
    let result = run(args)?;
    emit(&result)?;
    Ok(result.state == "completed" && result.exit_code == Some(0))
}

fn reserve_scope(
    helper: &crate::shared_writer::WriterContract,
    capacity_limit: usize,
    utility: bool,
    deadline: Instant,
    cancelled: &(impl Fn() -> bool + ?Sized),
) -> io::Result<LeanWorkerNativeScope> {
    let slot = NativeCapacitySlot {
        key: format!("native-reader:{}", uuid::Uuid::new_v4()),
        scope: if utility { "preparation" } else { "main" }.into(),
        limit: capacity_limit,
    };
    let admission_deadline = if utility {
        until(Duration::from_secs(30))?.min(deadline)
    } else {
        deadline
    };
    let socket_root = crate::native_socket::directory()?;
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "native utility admission cancelled",
            ));
        }
        match LeanWorkerNativeScope::prepare_with_socket_directory(
            &helper.ownership_registry,
            remaining(admission_deadline)?,
            Some(&slot),
            &socket_root,
        ) {
            Ok(scope) => return Ok(scope),
            Err(error) if utility && error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL.min(remaining(admission_deadline)?));
            }
            Err(error) => return Err(error),
        }
    }
}

fn run(args: &NativeReaderArgs) -> io::Result<NativeReaderResult> {
    let cancellation = match args
        .cancel_fd
        .map(control::CancellationPipe::from_inherited_fd)
        .transpose()
    {
        Ok(pipe) => pipe,
        Err(error) => return Ok(not_started(error)),
    };
    let mut spawned = false;
    match run_internal(
        args,
        None,
        None,
        None,
        None,
        &|| cancellation.as_ref().is_some_and(control::CancellationPipe::cancelled),
        &mut spawned,
    ) {
        Ok(value) => Ok(value),
        Err(error) if !spawned => Ok(not_started(error)),
        Err(error) => Err(error),
    }
}

fn run_internal(
    args: &NativeReaderArgs,
    program: Option<&NativeReaderProgram>,
    configured_helper: Option<&crate::shared_writer::WriterContract>,
    prepared_scope: Option<(LeanWorkerNativeScope, Instant)>,
    mut keepalive: Option<Box<dyn Send>>,
    cancelled: &(impl Fn() -> bool + ?Sized),
    spawned: &mut bool,
) -> io::Result<NativeReaderResult> {
    args.validate()?;
    if !(1..=128).contains(&args.capacity_limit) {
        return Err(invalid("native-reader capacity limit invalid"));
    }
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "native reader cancelled before spawn",
        ));
    }
    if let Some(program) = program {
        canonical(&program.cwd)?;
    }
    let deadline = match &prepared_scope {
        Some((_, deadline)) => *deadline,
        None => until(Duration::from_millis(args.timeout_millis))?,
    };
    let caller = if program.is_none() {
        let parent = rustix::process::getppid().ok_or_else(|| invalid("native-reader caller identity missing"))?;
        if parent.as_raw_nonzero().get() <= 1 {
            return Err(invalid("native-reader invoking caller ended before admission"));
        }
        let caller =
            NativeProcessIdentity::capture(parent.as_raw_nonzero().get().try_into().map_err(io::Error::other)?)?
                .ok_or_else(|| invalid("native-reader caller disappeared"))?;
        if rustix::process::getppid() != Some(parent) {
            return Err(invalid("native-reader caller changed during identity capture"));
        }
        Some(caller)
    } else {
        None
    };
    let mut helper = match configured_helper {
        Some(helper) => helper.clone(),
        None => crate::shared_writer::contract()?,
    };
    // The OS may report our own executable through /tmp's Darwin alias. The
    // selected helper is normalized; caller payload and project paths stay strict.
    helper.executable = helper.executable.canonicalize()?;
    canonical(&helper.executable)?;
    if helper.protocol != crate::shared_writer::PROTOCOL {
        return Err(invalid("native utility helper protocol differs"));
    }
    let scope = match prepared_scope {
        Some((scope, _)) => scope,
        None => reserve_scope(
            &helper,
            usize::from(args.capacity_limit),
            program.is_some(),
            deadline,
            cancelled,
        )?,
    };
    let staged_input = if args.stdin {
        let directory = scope.input_directory()?;
        Some(input::capture(&io::stdin(), &directory, deadline, &|| {
            Ok(!cancelled()
                && caller
                    .as_ref()
                    .map(NativeProcessIdentity::is_live)
                    .transpose()?
                    .unwrap_or(true))
        })?)
    } else {
        None
    };
    let mut command = Command::new(&helper.executable);
    command.arg("native-reader-exec");
    args.append(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(program) = program {
        command.envs(program.environment.iter().cloned());
    }
    for name in PRIVATE_ENV {
        command.env_remove(name);
    }
    if let Some(program) = program {
        command
            .env(PROGRAM_CWD_ENV, &program.cwd)
            .env(PROGRAM_ROLE_ENV, "preparation");
    }
    if let Some(staged) = &staged_input {
        command.env(STDIN_ENV, serde_json::to_string(staged).map_err(io::Error::other)?);
    }
    command.env("LEAN_RS_WORKER_GUARDIAN_EXECUTABLE", &helper.executable);
    let reaper = cleanup_reaper()?;
    let hello_timeout = remaining(deadline)?.min(Duration::from_secs(30));
    // An unwind inside startup cannot establish that no bootstrap was created.
    *spawned = true;
    let startup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scope.spawn(&mut command, hello_timeout).map_err(Box::new)
    }));
    let startup = startup.unwrap_or_else(|panic| {
        // No typed owner came back to certify closure. Fence these guards for
        // the remaining host lifetime, including when the utility catches the panic.
        if let Some(guard) = keepalive.take() {
            let _fenced = Box::leak(guard);
        }
        std::panic::resume_unwind(panic)
    });
    let lease = match startup {
        Ok(lease) => lease,
        Err(error) => match *error {
            LeanWorkerNativeSpawnError::BeforeSpawn(source) => {
                *spawned = false;
                return Err(source);
            }
            LeanWorkerNativeSpawnError::AfterSpawn { source, pending } => {
                let (closed, observation) = std::sync::mpsc::channel();
                let _handoff = reaper.send(CleanupJob::Pending {
                    pending,
                    keepalive,
                    closed,
                });
                return Ok(startup_failure(&source, observation.recv_timeout(CLEANUP).is_ok()));
            }
        },
    };
    let mut native = NativeGroupOwner {
        lease: Some(lease),
        keepalive,
        reaper,
        closed: false,
    };
    let mut output = Capture::new(
        native
            .lease()?
            .take_stdout()
            .ok_or_else(|| invalid("bootstrap stdout missing"))?,
        FRAME_LIMIT,
    )?;
    let mut diagnostics = Capture::new(
        native
            .lease()?
            .take_stderr()
            .ok_or_else(|| invalid("bootstrap stderr missing"))?,
        OUTPUT_LIMIT,
    )?;
    let mut interrupted = None;
    if !cancelled()
        && caller
            .as_ref()
            .map(NativeProcessIdentity::is_live)
            .transpose()?
            .unwrap_or(true)
        && Instant::now() < deadline
    {
        native.lease()?.acknowledge()?;
    } else {
        interrupted = Some("caller ended or deadline elapsed before payload admission".into());
        let _stop = native.lease()?.request_stop();
    }
    let mut cleanup_deadline = None;
    loop {
        output.poll()?;
        diagnostics.poll()?;
        let status = match native.lease()?.try_wait() {
            Ok(status) => status,
            Err(error) => {
                interrupted.get_or_insert_with(|| format!("native child wait authority lost: {error}"));
                None
            }
        };
        if interrupted.is_none() && status.is_none() && !native.lease()?.guardian_alive()? {
            interrupted = Some("native guardian exited before owned group closure".into());
        }
        if interrupted.is_none() {
            if Instant::now() >= deadline {
                interrupted = Some("native reader deadline elapsed".into());
            } else if cancelled() {
                interrupted = Some("native reader cancellation requested".into());
            } else if !caller
                .as_ref()
                .map(NativeProcessIdentity::is_live)
                .transpose()?
                .unwrap_or(true)
            {
                interrupted = Some("native reader invoking caller exited".into());
            }
        }
        if interrupted.is_some() {
            let _stop = native.lease()?.kill();
        }
        if (status.is_some() || interrupted.is_some() || native.lease()?.cleanup_pending())
            && cleanup_deadline.is_none()
        {
            cleanup_deadline = Some(Instant::now().checked_add(CLEANUP).unwrap_or_else(Instant::now));
            let _stop = native.lease()?.request_stop();
        }
        // A lost wait status cannot authorize signals or a usable result, but
        // independently observed group/custody closure still settles capacity.
        if native.lease()?.custody_closed()? {
            native.closed = true;
            let _inputs = native.lease()?.cleanup_inputs();
            match output.poll() {
                Ok(eof)
                    if args.stdout_result_frame
                        && (!eof || output.bytes.last() != Some(&b'\n') || !status.is_some_and(|s| s.success())) =>
                {
                    interrupted.get_or_insert_with(|| "native result frame or bootstrap exit incomplete".into());
                }
                Err(error) => {
                    interrupted
                        .get_or_insert_with(|| format!("native result capture failed after scope closure: {error}"));
                }
                Ok(_) => {}
            }
            let payload = if output.truncated {
                None
            } else {
                serde_json::from_slice::<PayloadResult>(&output.bytes)
                    .ok()
                    .filter(|payload| payload.protocol == PROTOCOL)
            };
            if payload.is_none() && interrupted.is_none() {
                diagnostics.poll()?;
                interrupted = Some(format!("no complete native payload report: {}", diagnostics.text()));
            }
            if let Some(staged) = &staged_input
                && payload.as_ref().is_some_and(|payload| payload.target_started)
                && !payload.as_ref().is_some_and(|payload| {
                    payload.stdin_sha256.as_deref() == Some(staged.sha256.as_str())
                        && payload.stdin_bytes == Some(staged.bytes)
                })
            {
                interrupted
                    .get_or_insert_with(|| "native stdin receipt differs from staged input; no usable result".into());
            }
            return Ok(result(payload, true, interrupted));
        }
        if cleanup_deadline.is_some_and(|limit| Instant::now() >= limit) {
            return Ok(result(
                None,
                false,
                interrupted.or_else(|| Some("native scope closure remains unconfirmed".into())),
            ));
        }
        thread::sleep(POLL);
    }
}

fn wait_child(child: &mut MeasuredChild, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

fn prepare_payload(args: &NativeReaderArgs, deadline: Instant) -> io::Result<(GuardianHandle, Vec<NativeReadDomain>)> {
    args.validate()?;
    rustix::process::setsid()?;
    let helper = crate::shared_writer::contract()?;
    let socket =
        std::env::var_os("LEAN_RS_WORKER_PARENT_SOCKET").ok_or_else(|| invalid("reader parent socket missing"))?;
    let nonce = std::env::var("LEAN_RS_WORKER_PARENT_NONCE").map_err(io::Error::other)?;
    let guardian_executable = std::env::var_os(lean_toolchain::native_guardian::EXECUTABLE_ENV)
        .ok_or_else(|| invalid("explicit native reader guardian executable missing"))?;
    canonical(Path::new(&guardian_executable))?;
    if !Path::new(&guardian_executable).is_file() {
        return Err(invalid("native reader guardian executable is not a file"));
    }
    let mut guardian = GuardianHandle::spawn(
        Path::new(&guardian_executable),
        Path::new(&socket),
        &nonce,
        remaining(deadline)?,
    )?;
    let roots = args.roots()?;
    // This fresh guardian has received no read loans yet. The union owner can
    // close every tentative SH before waiting, so a later-domain writer never
    // waits cyclically on an earlier partial read. Payload launch remains once.
    // Parent cancellation/lifeline stop owns this entire pre-body process group.
    let readers = SharedReader::acquire_union_with_ipc_root(
        &helper.executable,
        &roots,
        remaining(deadline)?,
        &|| false,
        &crate::native_socket::directory()?,
    )?;
    if args.roots()? != roots {
        return Err(invalid("native-reader artifact union changed during admission"));
    }
    let loans = roots
        .iter()
        .zip(&readers)
        .map(|(root, reader)| reader.guardian_loan(root))
        .collect::<io::Result<Vec<_>>>()?;
    guardian.retain_reads(&loans, remaining(deadline)?)?;
    let domains = loans.iter().map(|loan| loan.domain.clone()).collect();
    drop(loans);
    drop(readers);
    if args.roots()? != roots {
        return Err(invalid("native-reader artifact union changed before payload"));
    }
    Ok((guardian, domains))
}

fn verified_stdin(args: &NativeReaderArgs) -> io::Result<Option<(std::fs::File, input::StagedInput)>> {
    let supplied = std::env::var_os(STDIN_ENV);
    if !args.stdin {
        return if supplied.is_some() {
            Err(invalid("native stdin descriptor supplied without opt-in"))
        } else {
            Ok(None)
        };
    }
    let supplied = supplied.ok_or_else(|| invalid("native stdin descriptor missing"))?;
    let staged: input::StagedInput = serde_json::from_str(
        supplied
            .to_str()
            .ok_or_else(|| invalid("native stdin descriptor is not UTF-8"))?,
    )
    .map_err(io::Error::other)?;
    let registry = std::env::var_os(lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV)
        .ok_or_else(|| invalid("native stdin owner namespace missing"))?;
    let nonce = std::env::var("LEAN_RS_WORKER_PARENT_NONCE").map_err(io::Error::other)?;
    if nonce.len() != 48 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("native stdin generation missing"));
    }
    let expected = PathBuf::from(registry)
        .join(format!("worker-{nonce}"))
        .join("inputs")
        .join("stdin");
    if staged.path.as_os_str() != expected.as_os_str() {
        return Err(invalid("native stdin is outside the authenticated generation"));
    }
    let file = input::open_verified(&staged)?;
    Ok(Some((file, staged)))
}

// The Linux waiter has no children other than its payload and adopted descendants.
#[cfg(target_os = "linux")]
fn payload_has_no_survivors() -> io::Result<bool> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};
    // Include clone children with non-SIGCHLD exit signals. NOWAIT leaves
    // every status untouched. Only ECHILD establishes an empty selection;
    // a live or waitable child requires normal guardian cleanup.
    let options = WaitIdOptions::EXITED
        | WaitIdOptions::NOHANG
        | WaitIdOptions::NOWAIT
        | WaitIdOptions::from_bits_retain(libc::__WALL.cast_unsigned());
    match waitid(WaitId::All, options) {
        Err(rustix::io::Errno::CHILD) => Ok(true),
        Ok(_) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn payload_has_no_survivors() -> io::Result<bool> {
    lean_toolchain::process_group::current_group_is_singleton()
}

/// Controlled bootstrap only; called by `native-reader` after lifetime binding.
///
/// # Errors
/// Returns malformed private admission or result emission failures. It never
/// falls back to running the payload without a guardian.
pub fn run_exec(args: &NativeReaderArgs) -> io::Result<()> {
    let deadline = until(Duration::from_millis(args.timeout_millis))?;
    let mut payload = PayloadResult {
        protocol: PROTOCOL,
        ..PayloadResult::default()
    };
    let prepared = prepare_payload(args, deadline);
    let (_guardian, domains) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            payload.detail = Some(format!("native read admission refused before payload: {error}"));
            emit(&payload)?;
            return Ok(());
        }
    };
    payload.protected_domains = domains;
    let payload_cwd = match std::env::var_os(PROGRAM_CWD_ENV) {
        Some(cwd) if std::env::var(PROGRAM_ROLE_ENV).ok().as_deref() == Some("preparation") => {
            let cwd = PathBuf::from(cwd);
            canonical(&cwd)?;
            cwd
        }
        Some(_) => return Err(invalid("native reader private cwd lacks utility role")),
        None => args.project_root.clone(),
    };
    let payload_input = match verified_stdin(args) {
        Ok(input) => input,
        Err(error) => {
            payload.detail = Some(format!("native stdin refused before payload: {error}"));
            emit(&payload)?;
            return Ok(());
        }
    };
    let stdin = if let Some((file, staged)) = payload_input {
        payload.stdin_sha256 = Some(staged.sha256);
        payload.stdin_bytes = Some(staged.bytes);
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    #[cfg(not(target_os = "linux"))]
    let mut command = payload_command(args)?;
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut waiter = Command::new(std::env::current_exe()?);
        waiter.arg("native-reader-wait");
        let mut request = args.clone();
        request.timeout_millis = u64::try_from(remaining(deadline)?.as_millis()).map_err(io::Error::other)?;
        request.append(&mut waiter);
        waiter
    };
    command.current_dir(payload_cwd).stdin(stdin);
    for name in PRIVATE_ENV {
        command.env_remove(name);
    }
    #[cfg(not(target_os = "linux"))]
    measure_payload(command, args, deadline, &mut payload)?;
    #[cfg(target_os = "linux")]
    {
        let mut report = wait_for_payload_report(command, deadline)?;
        report.protected_domains = payload.protected_domains;
        report.stdin_sha256 = payload.stdin_sha256;
        report.stdin_bytes = payload.stdin_bytes;
        payload = report;
    }
    emit(&payload)
}

fn payload_command(args: &NativeReaderArgs) -> io::Result<Command> {
    let executable = args
        .command
        .first()
        .ok_or_else(|| invalid("payload executable missing"))?;
    let mut command = Command::new(executable);
    command.args(args.command.iter().skip(1));
    Ok(command)
}

#[cfg(target_os = "linux")]
fn wait_for_payload_report(mut command: Command, deadline: Instant) -> io::Result<PayloadResult> {
    // The payload gets separate captured stdout/stderr. Only the waiter writes
    // this pipe. Its own stderr goes to the bootstrap's bounded diagnostics.
    command.stdout(Stdio::piped());
    let mut waiter = ChildOwner::spawn_measured(command)?;
    let pipe = waiter
        .get()?
        .take_stdout()
        .ok_or_else(|| invalid("missing waiter report pipe"))?;
    let mut report = Capture::new(pipe, FRAME_LIMIT)?;
    loop {
        let eof = report.poll()?;
        if let Some(status) = waiter.get()?.try_wait()? {
            if !status.success() {
                return Err(invalid(format!("native payload waiter exited {status}")));
            }
            if eof {
                remaining(deadline)?;
                break;
            }
        }
        remaining(deadline)?;
        thread::sleep(POLL);
    }
    if report.truncated || report.bytes.last() != Some(&b'\n') {
        return Err(invalid("incomplete or oversized native payload waiter report"));
    }
    let payload: PayloadResult = serde_json::from_slice(&report.bytes).map_err(io::Error::other)?;
    if payload.protocol != PROTOCOL {
        return Err(invalid("invalid native payload waiter protocol"));
    }
    Ok(payload)
}

/// Measure one payload for its owning bootstrap. The private report carries no custody receipt.
///
/// # Errors
/// Fails on invalid bootstrap context, loss of wait authority, or incomplete output.
#[cfg(target_os = "linux")]
pub fn run_waiter(args: &NativeReaderArgs) -> io::Result<()> {
    use rustix::process::{Pid, getpgrp, getppid, getsid, set_child_subreaper};
    if getppid() != Some(getpgrp()) || getsid(None)? != getpgrp() {
        return Err(invalid("payload waiter requires its private session leader as parent"));
    }
    args.validate()?;
    set_child_subreaper(Some(Pid::INIT))?;
    let mut payload = PayloadResult {
        protocol: PROTOCOL,
        ..PayloadResult::default()
    };
    measure_payload(
        payload_command(args)?,
        args,
        until(Duration::from_millis(args.timeout_millis))?,
        &mut payload,
    )?;
    emit(&payload)
}

fn measure_payload(
    mut command: Command,
    args: &NativeReaderArgs,
    deadline: Instant,
    payload: &mut PayloadResult,
) -> io::Result<()> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    match ChildOwner::spawn_measured(command) {
        Err(error) => payload.detail = Some(format!("payload spawn failed: {error}")),
        Ok(mut child) => {
            payload.target_started = true;
            let mut output = Capture::new(
                child
                    .get()?
                    .take_stdout()
                    .ok_or_else(|| invalid("payload stdout missing"))?,
                args.max_output_bytes,
            )?;
            let mut diagnostics = Capture::new(
                child
                    .get()?
                    .take_stderr()
                    .ok_or_else(|| invalid("payload stderr missing"))?,
                args.max_output_bytes,
            )?;
            loop {
                output.poll()?;
                diagnostics.poll()?;
                if let Some(status) = child.get()?.try_wait()? {
                    let stdout_closed = output.poll()?;
                    diagnostics.poll()?;
                    payload.exit_code = status.code();
                    payload.exit_status = Some(status.to_string());
                    payload.payload_usage = child.get()?.usage().and_then(PayloadUsage::from_waited);
                    payload.intervened = !payload_has_no_survivors()?;
                    if args.stdout_result_frame && !stdout_closed {
                        payload.output_truncated = true;
                        payload.detail = Some("payload stdout incomplete at exit".into());
                    }
                    if payload.intervened {
                        payload.detail = Some("payload descendant still present at verdict; no usable result".into());
                    }
                    break;
                }
                remaining(deadline)?;
                thread::sleep(POLL);
            }
            payload.stdout = output.text();
            payload.stderr = diagnostics.text();
            payload.output_truncated |= output.truncated || (!args.stdout_result_frame && diagnostics.truncated);
            if args.stdout_result_frame && payload.detail.is_none() {
                if output.truncated {
                    payload.detail = Some("required payload stdout exceeds capture limit".into());
                } else if diagnostics.truncated {
                    payload.detail = Some("payload stderr diagnostic clipped".into());
                }
            }
        }
    }
    Ok(())
}

fn emit(value: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() > FRAME_LIMIT {
        return Err(invalid("native-reader reply exceeds limit"));
    }
    let output = io::stdout();
    fcntl_setfl(&output, fcntl_getfl(&output)? | OFlags::NONBLOCK)?;
    let deadline = until(CLEANUP)?;
    let mut pending = bytes.as_slice();
    while !pending.is_empty() {
        let budget = remaining(deadline)?;
        match rustix::io::write(&output, pending) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "native-reader reply accepted no bytes",
                ));
            }
            Ok(count) => {
                pending = pending
                    .get(count..)
                    .ok_or_else(|| invalid("native-reader reply count invalid"))?;
            }
            Err(error) if error == rustix::io::Errno::AGAIN || error == rustix::io::Errno::INTR => {
                thread::sleep(POLL.min(budget));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod program_tests;
