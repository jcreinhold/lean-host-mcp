//! Runtime-free artifact preparation, before any guarded native generation.
//!
//! One pre-created thread owns the actual preparation child from spawn through
//! reap. Its exit is independent of writer helpers started by preparation:
//! failures after spawn therefore remain Unknown even when this child stopped.

use std::fmt;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV;
use lean_toolchain::shared_writer::{WRITER_HANDOFF_ENV, WriterDisposition, configure_writer_executable};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

const STDOUT_LIMIT: usize = 64 * 1024;
const STDERR_LIMIT: usize = 32 * 1024;
const PATH_LIMIT: usize = 32;
const POLL: Duration = Duration::from_millis(10);
const CALLER_POLL: Duration = Duration::from_millis(20);
const NOT_READY_EXIT: i32 = 75;
const CLEANUP_WAIT: Duration = Duration::from_millis(250);
// Keep the shared owners-directory registry binding: preparation writers must
// consult the same orphan/fence namespace as native generations. Only remove
// inherited lifetime activation and private capabilities here.
const SCRUB_ENV: &[&str] = &[
    "LEAN_RS_WORKER_PARENT_SOCKET",
    "LEAN_RS_WORKER_PARENT_NONCE",
    "LEAN_RS_WORKER_GUARDIAN_EXECUTABLE",
    "LEAN_RS_NATIVE_GUARDIAN_CONTROL_FD",
    "LEAN_RS_NATIVE_GUARDIAN_WORKER_PID",
    "LEAN_RS_WORKER_SHARED_ARTIFACT_READS",
    "LEAN_RS_PREPARED_HOST_ARTIFACTS",
];

/// Failed preparation supplies no native check result or independent-H exit proof.
#[derive(Debug)]
pub(crate) struct PreparationFailure {
    pub(crate) message: String,
    /// Snapshot of an actual wait on this child, not a late-reap observer and
    /// never evidence that independent writer scopes stopped.
    pub(crate) preparation_child_stopped: bool,
    pub(crate) disposition: WriterDisposition,
}

impl fmt::Display for PreparationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for PreparationFailure {}

impl PreparationFailure {
    fn no_spawn(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            preparation_child_stopped: false,
            disposition: WriterDisposition::NotStarted,
        }
    }

    fn unknown(message: impl fmt::Display, stopped: bool) -> Self {
        Self {
            message: message.to_string(),
            preparation_child_stopped: stopped,
            disposition: WriterDisposition::Unknown,
        }
    }
}

/// Probe existing artifacts without building, writing or loading native code.
///
/// `None` requires the read-only worker mode's explicit not-ready exit, observed
/// on the actual child. This call never falls through to artifact preparation.
///
/// # Errors
/// Returns bounded launch, capture, cancellation or artifact-validation failures.
pub(crate) fn probe_ready(
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<LeanWorkerPreparedHostArtifacts>, PreparationFailure> {
    run(worker, sysroot, writer, timeout, cancelled, Mode::Ready)
}

/// Prepare storage facts without entering the worker's native runtime.
///
/// The caller must cancel its token when abandoning an async waiter: dropping a
/// `spawn_blocking` handle alone cannot interrupt its running closure. The token
/// is polled while the independent owner retains and reaps the actual child.
///
/// # Errors
/// Returns bounded launch, capture, cancellation or artifact-validation failures.
/// A stopped preparation child cannot certify closure of independent writers.
pub(crate) fn prepare_cancellable(
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
) -> Result<LeanWorkerPreparedHostArtifacts, PreparationFailure> {
    run(worker, sysroot, writer, timeout, cancelled, Mode::Prepare)?
        .ok_or_else(|| PreparationFailure::unknown("preparation returned no artifacts", true))
}

#[cfg(test)]
fn prepare(
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    timeout: Duration,
) -> Result<LeanWorkerPreparedHostArtifacts, PreparationFailure> {
    prepare_cancellable(worker, sysroot, writer, timeout, &|| false)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Prepare,
    Ready,
}

type PreparationResult = Result<Option<LeanWorkerPreparedHostArtifacts>, PreparationFailure>;
type Reply = Option<SyncSender<PreparationResult>>;

fn run(
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
    mode: Mode,
) -> PreparationResult {
    if cancelled() {
        return Err(PreparationFailure::no_spawn("host preparation cancelled before launch"));
    }
    let registry = crate::shared_writer::ownership_registry().map_err(PreparationFailure::no_spawn)?;
    let mut command = Command::new(worker);
    configure(&mut command, sysroot, writer, &registry, mode).map_err(PreparationFailure::no_spawn)?;
    start(command, timeout, mode)?.wait_cancellable(cancelled)
}

fn configure(command: &mut Command, sysroot: &Path, writer: &Path, registry: &Path, mode: Mode) -> io::Result<()> {
    command
        .arg(match mode {
            Mode::Prepare => "--lean-rs-prepare-host-artifacts",
            Mode::Ready => "--lean-rs-ready-host-artifacts",
        })
        .env("LEAN_SYSROOT", sysroot)
        .env(OWNERS_DIRECTORY_ENV, registry)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in SCRUB_ENV.iter().chain(WRITER_HANDOFF_ENV.iter()) {
        command.env_remove(name);
    }
    crate::toolchain::pass_elan_home(command);
    configure_writer_executable(command, Some(writer))
}

#[derive(Default)]
struct Control {
    cancelled: AtomicBool,
    child_stopped: AtomicBool,
}

struct Pending {
    receive: Receiver<PreparationResult>,
    control: Arc<Control>,
    deadline: Instant,
}

impl Pending {
    #[cfg(test)]
    fn wait(self) -> PreparationResult {
        self.wait_cancellable(&|| false)
    }

    fn wait_cancellable(self, cancelled: &impl Fn() -> bool) -> PreparationResult {
        let cleanup_margin = CLEANUP_WAIT.saturating_add(CALLER_POLL);
        let mut wait_end = self.deadline.checked_add(cleanup_margin).unwrap_or(self.deadline);
        let mut caller_cancelled = false;
        loop {
            if !caller_cancelled && cancelled() {
                caller_cancelled = true;
                self.control.cancelled.store(true, Ordering::Release);
                let cancellation_end = Instant::now().checked_add(cleanup_margin).unwrap_or(wait_end);
                wait_end = wait_end.min(cancellation_end);
            }
            let remaining = wait_end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PreparationFailure::unknown(
                    "host artifact preparation wait exceeded cleanup bound",
                    self.control.child_stopped.load(Ordering::Acquire),
                ));
            }
            match self.receive.recv_timeout(remaining.min(CALLER_POLL)) {
                Ok(Ok(_)) if caller_cancelled => {
                    return Err(PreparationFailure::unknown(
                        "host artifact preparation cancelled",
                        self.control.child_stopped.load(Ordering::Acquire),
                    ));
                }
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(error @ RecvTimeoutError::Disconnected) => {
                    return Err(PreparationFailure::unknown(
                        format!("host artifact preparation wait ended: {error}"),
                        self.control.child_stopped.load(Ordering::Acquire),
                    ));
                }
            }
        }
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        // Only signal the actual owner; never drop or reap its Child here.
        self.control.cancelled.store(true, Ordering::Release);
    }
}

fn start(command: Command, timeout: Duration, mode: Mode) -> Result<Pending, PreparationFailure> {
    if timeout.is_zero() {
        return Err(PreparationFailure::no_spawn(
            "host preparation needs a positive timeout",
        ));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| PreparationFailure::no_spawn("host preparation deadline is too large"))?;
    let control = Arc::new(Control::default());
    let owner_control = Arc::clone(&control);
    let (send, receive) = mpsc::sync_channel(1);
    // Thread creation precedes spawn: no Child exists if this operation fails.
    thread::Builder::new()
        .name("lean-host-preparation-owner".to_owned())
        .spawn(move || own(command, deadline, owner_control.as_ref(), send, mode))
        .map_err(PreparationFailure::no_spawn)?;
    Ok(Pending {
        receive,
        control,
        deadline,
    })
}

struct Capture<T> {
    input: T,
    bytes: Vec<u8>,
    limit: usize,
    name: &'static str,
    eof: bool,
}
impl<T: Read + AsFd> Capture<T> {
    fn new(input: T, limit: usize, name: &'static str) -> io::Result<Self> {
        fcntl_setfl(&input, fcntl_getfl(&input)? | OFlags::NONBLOCK)?;
        Ok(Self {
            input,
            bytes: Vec::new(),
            limit,
            name,
            eof: false,
        })
    }

    fn poll(&mut self, retain: bool) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        // Cap each poll's work so continuous output cannot starve the deadline,
        // the other pipe, cancellation or wait/kill observation.
        for _ in 0..4 {
            let mut buffer = [0_u8; 4096];
            match self.input.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(count) => {
                    if !retain {
                        continue;
                    }
                    let remaining = self.limit.saturating_sub(self.bytes.len());
                    let kept = count.min(remaining);
                    self.bytes.extend_from_slice(
                        buffer
                            .get(..kept)
                            .ok_or_else(|| io::Error::other("invalid pipe read count"))?,
                    );
                    if count > remaining {
                        return Err(io::Error::other(format!(
                            "preparation {} exceeded {} bytes",
                            self.name, self.limit
                        )));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

type Pipes = (Capture<ChildStdout>, Capture<ChildStderr>);

fn captures(child: &mut Child) -> io::Result<Pipes> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("preparation stdout missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("preparation stderr missing"))?;
    Ok((
        Capture::new(stdout, STDOUT_LIMIT, "stdout")?,
        Capture::new(stderr, STDERR_LIMIT, "stderr")?,
    ))
}

fn deliver(reply: &mut Reply, result: PreparationResult) {
    // Exactly one message enters a previously empty one-slot channel. Delivery
    // therefore cannot block cleanup, including after the recipient disappears.
    if let Some(sender) = reply.take() {
        let _delivered = sender.send(result);
    }
}

fn own(mut command: Command, deadline: Instant, control: &Control, send: SyncSender<PreparationResult>, mode: Mode) {
    let mut reply = Some(send);
    if control.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        deliver(
            &mut reply,
            Err(PreparationFailure::no_spawn("preparation cancelled before spawn")),
        );
        return;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            deliver(&mut reply, Err(PreparationFailure::no_spawn(error)));
            return;
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive(&mut child, deadline, control, &mut reply, mode);
    }));
    if result.is_err() {
        // The Child remains outside the unwinding frame, so panic cannot drop
        // its ownership. Continue bounded caller notification and actual reap.
        retire(&mut child, control, &mut reply, "preparation owner panicked", None);
    }
}

fn observe(child: &mut Child, control: &Control) -> io::Result<Option<ExitStatus>> {
    let status = child.try_wait()?;
    if status.is_some() {
        control.child_stopped.store(true, Ordering::Release);
    }
    Ok(status)
}

fn drive(child: &mut Child, deadline: Instant, control: &Control, reply: &mut Reply, mode: Mode) {
    let mut pipes = match captures(child) {
        Ok(pipes) => pipes,
        Err(error) => {
            retire(
                child,
                control,
                reply,
                &format!("preparation capture setup failed: {error}"),
                None,
            );
            return;
        }
    };
    loop {
        let stdout = pipes.0.poll(true);
        let stderr = pipes.1.poll(true);
        let captured = stdout.and(stderr);
        if let Err(error) = captured {
            retire(child, control, reply, &error.to_string(), Some(&mut pipes));
            return;
        }
        if control.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            // Keep both pipes owned while actual-child cleanup is pending.
            retire(
                child,
                control,
                reply,
                "host artifact preparation cancelled or timed out",
                Some(&mut pipes),
            );
            return;
        }
        match observe(child, control) {
            Ok(Some(status)) if mode == Mode::Ready && status.code() == Some(NOT_READY_EXIT) => {
                if pipes.0.eof && pipes.1.eof {
                    deliver(reply, Ok(None));
                    return;
                }
            }
            Ok(Some(status)) if !status.success() => {
                deliver(
                    reply,
                    Err(PreparationFailure::unknown(
                        format!(
                            "host artifact preparation exited {status}: {}",
                            String::from_utf8_lossy(&pipes.1.bytes)
                        ),
                        true,
                    )),
                );
                return;
            }
            Ok(Some(_)) if pipes.0.eof && pipes.1.eof => {
                let result = validate(&pipes.0.bytes)
                    .map(Some)
                    .map_err(|error| PreparationFailure::unknown(error, true));
                deliver(reply, result);
                return;
            }
            Ok(_) => {}
            Err(error) => {
                retire(
                    child,
                    control,
                    reply,
                    &format!("preparation wait failed: {error}"),
                    Some(&mut pipes),
                );
                return;
            }
        }
        thread::sleep(POLL);
    }
}

fn retire(child: &mut Child, control: &Control, reply: &mut Reply, message: &str, mut pipes: Option<&mut Pipes>) {
    let started = Instant::now();
    loop {
        if let Some(pipes) = pipes.as_deref_mut() {
            // A vanished recipient never applies backpressure to actual work.
            let _stdout = pipes.0.poll(false);
            let _stderr = pipes.1.poll(false);
        }
        match observe(child, control) {
            Ok(Some(_)) => {
                deliver(reply, Err(PreparationFailure::unknown(message, true)));
                return;
            }
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) => {
                // An external reaper revoked ownership. Never signal a reused PID.
                deliver(
                    reply,
                    Err(PreparationFailure::unknown(
                        format!("{message}; child custody lost"),
                        false,
                    )),
                );
                return;
            }
            Ok(None) | Err(_) => {}
        }
        let _kill = child.kill(); // A kill request alone is never an exit witness.
        if started.elapsed() >= CLEANUP_WAIT {
            deliver(reply, Err(PreparationFailure::unknown(message, false)));
        }
        thread::sleep(POLL);
    }
}

fn validate(bytes: &[u8]) -> Result<LeanWorkerPreparedHostArtifacts, String> {
    if bytes.len() > STDOUT_LIMIT {
        return Err("preparation DTO exceeds the 64 KiB limit".to_owned());
    }
    let artifacts: LeanWorkerPreparedHostArtifacts =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid preparation DTO: {error}"))?;
    if artifacts.protocol != 1 {
        return Err("unsupported preparation protocol".to_owned());
    }
    for paths in [&artifacts.artifact_roots, &artifacts.shim_search_paths] {
        if paths.is_empty() || paths.len() > PATH_LIMIT {
            return Err("preparation requires one to 32 paths per array".to_owned());
        }
        for path in paths {
            canonical(Path::new(path), true)?;
        }
    }
    let manifest = Path::new(&artifacts.shim_manifest_path);
    canonical(manifest, false)?;
    let mut actual = Vec::new();
    std::fs::File::open(manifest)
        .map_err(|error| error.to_string())?
        .take(
            u64::try_from(STDOUT_LIMIT)
                .map_err(|error| error.to_string())?
                .saturating_add(1),
        )
        .read_to_end(&mut actual)
        .map_err(|error| error.to_string())?;
    if actual != artifacts.shim_manifest_json.as_bytes() {
        return Err("prepared manifest snapshot differs from its exact disk bytes".to_owned());
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&actual).map_err(|error| format!("invalid shim manifest JSON: {error}"))?;
    if !manifest.is_object() {
        return Err("shim manifest must be a JSON object".to_owned());
    }
    Ok(artifacts)
}

fn canonical(path: &Path, directory: bool) -> Result<(), String> {
    let actual = path.canonicalize().map_err(|error| error.to_string())?;
    if !path.is_absolute()
        || actual.as_os_str() != path.as_os_str()
        || (directory && !actual.is_dir())
        || (!directory && !actual.is_file())
    {
        return Err("preparation paths must be canonical existing files/directories".to_owned());
    }
    Ok(())
}

#[cfg(test)]
#[path = "host_preparation/tests.rs"]
mod tests;
