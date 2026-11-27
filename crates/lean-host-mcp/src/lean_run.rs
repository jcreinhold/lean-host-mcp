//! Short runs: a Lean or Lake program the server starts for one request, such
//! as `lake setup-file`, a file check or the module options reader.
//!
//! Requirement: a short run's reply never waits on a process outside its
//! group. Each run starts as a direct child in its own process group
//! ([`OwnedCommandGroup`]), with its own stdout and stderr pipes and its own
//! child file ([`ChildFile`]). The server reads both pipes while the group
//! runs. Once the group has exited it reads only what the pipes still hold and
//! never waits for end of file: on macOS a pipe made while another thread
//! spawns a child can leak its write end into that child, so end of file may
//! never come.
//!
//! A run stops its whole group when the request is cancelled, when its time
//! runs out, when output passes its cap, or when its leader exits and leaves
//! other members running. A group that has not exited [`STOP_GRACE`] after the
//! stop goes to a thread that keeps stopping it, and holds the run's inputs,
//! child file and keepalive until it exits; the reply says the outcome is
//! unknown.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, PipeReader, Read as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lean_toolchain::process_group::{OwnedCommandGroup, group_present};
use parking_lot::Mutex;
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

use crate::children::ChildFile;

/// The cap on stderr, and on stdout unless a program names another.
pub(crate) const OUTPUT_LIMIT: usize = 1024 * 1024;

/// The stdout cap for `lake setup-file`, whose JSON names every import.
pub(crate) const PROGRAM_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// How long the loop waits for output before it checks the group again.
const POLL: Duration = Duration::from_millis(10);

/// How long a leader that exited may leave other members of its group
/// running before the run stops them.
const LEFTOVER_GRACE: Duration = Duration::from_secs(1);

/// How long a stopped group may take to exit before the reply goes out
/// without it.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);

/// How often the late thread checks a group that outlived its reply.
const LATE_POLL: Duration = Duration::from_millis(100);

/// One program to run, prepared in the run's private input directory.
#[derive(Debug)]
pub(crate) struct Program {
    /// The program and its arguments.
    pub(crate) command: Vec<OsString>,
    /// Variables set on top of the server's environment.
    pub(crate) environment: Vec<(OsString, OsString)>,
    pub(crate) cwd: PathBuf,
    pub(crate) max_stdout_bytes: usize,
    /// Bytes the program reads as stdin; without them it reads nothing.
    pub(crate) stdin: Option<Vec<u8>>,
}

/// What a failed run says about the program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    /// The program never started.
    NotStarted,
    /// The program's group has exited without a usable result.
    Stopped,
    /// The program's group may still run; a late thread keeps stopping it.
    Unknown,
}

/// A run that gave no usable result.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) disposition: Disposition,
    pub(crate) detail: String,
}

impl Failure {
    fn not_started(detail: impl Into<String>) -> Self {
        Self {
            disposition: Disposition::NotStarted,
            detail: detail.into(),
        }
    }

    fn stopped(detail: impl Into<String>) -> Self {
        Self {
            disposition: Disposition::Stopped,
            detail: detail.into(),
        }
    }
}

/// A program that exited on its own, with all it wrote.
#[derive(Debug)]
pub(crate) struct Exit {
    pub(crate) code: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

/// Run the program `prepare` writes into a fresh input directory, for at most
/// `timeout`, stopping it when `cancelled` turns true. `keepalive` lives until
/// the program's group has exited, even past the reply.
///
/// # Errors
///
/// Returns a [`Failure`] when the program does not start, when its group was
/// stopped or its output was cut, when it ends by a signal, or when its group
/// does not exit within [`STOP_GRACE`] of a stop.
pub(crate) fn run(
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    keepalive: Box<dyn Send>,
    prepare: impl FnOnce(&Path) -> io::Result<Program>,
) -> Result<Exit, Failure> {
    let started = Instant::now();
    if cancelled() {
        return Err(Failure::not_started("cancelled before the program started"));
    }
    if timeout.is_zero() {
        return Err(Failure::not_started("the deadline passed before the program started"));
    }
    let inputs =
        inputs_dir().map_err(|error| Failure::not_started(format!("cannot create the run's inputs: {error}")))?;
    let program = prepare(inputs.path())
        .map_err(|error| Failure::not_started(format!("cannot write the run's inputs: {error}")))?;
    let name = program
        .command
        .first()
        .map(|program| program.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (command, mut stdout, mut stderr) = command(&program, inputs.path())
        .map_err(|error| Failure::not_started(format!("cannot set up {name}: {error}")))?;
    let mut child_file = ChildFile::create(inputs.path())
        .map_err(|error| Failure::not_started(format!("cannot record {name}: {error}")))?;
    let inherited: Vec<BorrowedFd<'_>> = child_file.as_fd().into_iter().collect();
    // The command, and with it this process's write ends of the pipes, goes
    // away here; only the child holds them now.
    let spawned = OwnedCommandGroup::spawn_inheriting(command, &inherited);
    drop(inherited);
    let mut group = spawned.map_err(|error| Failure::not_started(format!("cannot start {name}: {error}")))?;
    child_file.started(group.id());

    let deadline = started.checked_add(timeout);
    let mut stop: Option<(String, Instant)> = None;
    let mut leftover: Option<Instant> = None;
    let mut read_error: Option<io::Error> = None;
    loop {
        for capture in [&mut stdout, &mut stderr] {
            if let Err(error) = capture.poll() {
                read_error.get_or_insert(error);
            }
        }
        match group.try_wait() {
            Ok(Some(status)) => {
                // The group has exited: take what the pipes hold, then stop.
                for capture in [&mut stdout, &mut stderr] {
                    if let Err(error) = capture.drain() {
                        read_error.get_or_insert(error);
                    }
                }
                drop((child_file, inputs, keepalive));
                return finish(
                    &name,
                    status,
                    stop.map(|(reason, _)| reason),
                    read_error,
                    stdout,
                    &stderr,
                );
            }
            Ok(None) => {}
            Err(error) => {
                let detail = format!("lost track of {name}'s process group: {error}");
                return Err(Late::abandon(group, child_file, inputs, keepalive, detail));
            }
        }
        if let Some((reason, at)) = &stop {
            // The SDK stops the group while its leader runs; this also stops
            // members the leader left behind.
            if let Err(error) = child_file.stop_group(group.id()) {
                tracing::warn!(group = group.id(), %error, "cannot stop the rest of a short run's process group");
            }
            if at.elapsed() >= STOP_GRACE {
                let detail = format!(
                    "{reason}; {name}'s process group has not exited {} s after the stop",
                    STOP_GRACE.as_secs()
                );
                return Err(Late::abandon(group, child_file, inputs, keepalive, detail));
            }
        } else if let Some(reason) = stop_reason(
            &cancelled,
            deadline,
            timeout,
            &group,
            &mut leftover,
            &stdout,
            &stderr,
            read_error.as_ref(),
        ) {
            if let Err(error) = group.request_stop() {
                let detail = format!("{reason}; cannot stop {name}: {error}");
                return Err(Late::abandon(group, child_file, inputs, keepalive, detail));
            }
            stop = Some((reason, Instant::now()));
        }
        wait_readable(&[stdout.awaited(), stderr.awaited()], POLL);
    }
}

/// Why a running group must stop now, if it must.
fn stop_reason(
    cancelled: &impl Fn() -> bool,
    deadline: Option<Instant>,
    timeout: Duration,
    group: &OwnedCommandGroup,
    leftover: &mut Option<Instant>,
    stdout: &Capture,
    stderr: &Capture,
    read_error: Option<&io::Error>,
) -> Option<String> {
    if cancelled() {
        return Some("the request was cancelled".to_owned());
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Some(format!("the run passed its {} s limit", timeout.as_secs_f64()));
    }
    if stdout.truncated || stderr.truncated {
        return Some(format!(
            "the program wrote more than {} bytes of stdout or {} bytes of stderr",
            stdout.limit, stderr.limit
        ));
    }
    if let Some(error) = read_error {
        return Some(format!("cannot read the program's output: {error}"));
    }
    if group.cleanup_pending() && leftover.get_or_insert_with(Instant::now).elapsed() >= LEFTOVER_GRACE {
        return Some("the program exited and left processes running in its group".to_owned());
    }
    None
}

/// The result of a run whose group has exited.
fn finish(
    name: &str,
    status: ExitStatus,
    stop: Option<String>,
    read_error: Option<io::Error>,
    stdout: Capture,
    stderr: &Capture,
) -> Result<Exit, Failure> {
    let stderr_text = stderr.text();
    let with_stderr = |detail: String| {
        if stderr_text.is_empty() {
            detail
        } else {
            format!("{detail}: {stderr_text}")
        }
    };
    if let Some(reason) = stop {
        return Err(Failure::stopped(with_stderr(format!(
            "{reason}; the server stopped {name}"
        ))));
    }
    if let Some(error) = read_error {
        return Err(Failure::stopped(with_stderr(format!(
            "cannot read {name}'s output: {error}"
        ))));
    }
    if stdout.truncated || stderr.truncated {
        return Err(Failure::stopped(with_stderr(format!(
            "{name} wrote more output than its cap"
        ))));
    }
    let Some(code) = status.code() else {
        let signal = status.signal().unwrap_or_default();
        return Err(Failure::stopped(with_stderr(format!(
            "{name} ended by signal {signal}"
        ))));
    };
    Ok(Exit {
        code,
        stdout: stdout.bytes,
        stderr: stderr_text,
    })
}

/// The command for `program`, with its stdin and its own output pipes.
fn command(program: &Program, inputs: &Path) -> io::Result<(Command, Capture, Capture)> {
    let (executable, arguments) = program
        .command
        .split_first()
        .ok_or_else(|| io::Error::other("empty command"))?;
    let mut command = Command::new(executable);
    command.args(arguments).current_dir(&program.cwd);
    for (key, value) in &program.environment {
        command.env(key, value);
    }
    match &program.stdin {
        Some(bytes) => {
            let path = inputs.join("stdin");
            fs::write(&path, bytes)?;
            command.stdin(File::open(&path)?);
        }
        None => {
            command.stdin(Stdio::null());
        }
    }
    let (stdout, stdout_writer) = io::pipe()?;
    let (stderr, stderr_writer) = io::pipe()?;
    command.stdout(stdout_writer).stderr(stderr_writer);
    Ok((
        command,
        Capture::new(stdout, program.max_stdout_bytes)?,
        Capture::new(stderr, OUTPUT_LIMIT)?,
    ))
}

/// A private directory for one run's inputs: under `runs/` in the store this
/// process holds, else under the system's temporary directory.
fn inputs_dir() -> io::Result<tempfile::TempDir> {
    match crate::store::process_store() {
        Some(store) => {
            let runs = store.join(crate::store::RUNS_DIR_NAME);
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&runs)?;
            tempfile::Builder::new().prefix("run-").tempdir_in(runs)
        }
        None => tempfile::Builder::new().prefix("lean-host-mcp-run-").tempdir(),
    }
}

/// What a run whose group outlived its reply leaves behind.
struct Late {
    group: OwnedCommandGroup,
    file: ChildFile,
    /// Kept so the inputs outlive the group.
    _inputs: tempfile::TempDir,
    /// Kept so the request's turn share and permit outlive the group.
    _keepalive: Box<dyn Send>,
}

impl Late {
    /// Leave a group that outlived its reply to a late thread, and say so.
    fn abandon(
        group: OwnedCommandGroup,
        file: ChildFile,
        inputs: tempfile::TempDir,
        keepalive: Box<dyn Send>,
        detail: String,
    ) -> Failure {
        Self {
            group,
            file,
            _inputs: inputs,
            _keepalive: keepalive,
        }
        .hand_off();
        Failure {
            disposition: Disposition::Unknown,
            detail,
        }
    }

    /// Hand this to a thread that keeps stopping the group until it exits.
    /// Without a thread, do that here.
    fn hand_off(self) {
        let state = Arc::new(Mutex::new(Some(self)));
        let handed = Arc::clone(&state);
        let spawned = thread::Builder::new().name("lean-run-late".into()).spawn(move || {
            let late = handed.lock().take();
            if let Some(late) = late {
                late.wait();
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start a thread to stop a short run; stopping it in the request");
            let late = state.lock().take();
            if let Some(late) = late {
                late.wait();
            }
        }
    }

    fn wait(mut self) {
        let group = self.group.id();
        loop {
            match self.group.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => self.stop_rest(),
                Err(_) => break,
            }
        }
        // The SDK lost track of the leader; watch the group, and signal it
        // only while its child file shows a member still runs.
        loop {
            match group_present(group) {
                Ok(false) => return,
                Ok(true) => self.stop_rest(),
                Err(error) => {
                    tracing::warn!(
                        group, %error,
                        "cannot watch a short run's process group; the next server waits for it"
                    );
                    self.file.keep();
                    return;
                }
            }
        }
    }

    fn stop_rest(&self) {
        if let Err(error) = self.file.stop_group(self.group.id()) {
            tracing::warn!(group = self.group.id(), %error, "cannot stop the rest of a short run's process group");
        }
        thread::sleep(LATE_POLL);
    }
}

/// One output pipe of a run, read without blocking up to its cap.
#[derive(Debug)]
pub(crate) struct Capture {
    reader: PipeReader,
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    closed: bool,
}

impl Capture {
    /// Capture `reader`, keeping at most `limit` bytes.
    pub(crate) fn new(reader: PipeReader, limit: usize) -> io::Result<Self> {
        let flags = fcntl_getfl(&reader)?;
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK)?;
        Ok(Self {
            reader,
            bytes: Vec::new(),
            limit,
            truncated: false,
            closed: false,
        })
    }

    /// Read what the pipe holds now, up to a bound per call so a program that
    /// writes without pause cannot hold the loop. Returns whether more may
    /// wait.
    pub(crate) fn poll(&mut self) -> io::Result<bool> {
        if self.closed {
            return Ok(false);
        }
        let mut buffer = [0_u8; 8192];
        for _ in 0..128 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(false);
                }
                Ok(count) => {
                    let accepted = count.min(self.limit.saturating_sub(self.bytes.len()));
                    let kept = buffer
                        .get(..accepted)
                        .ok_or_else(|| io::Error::other("read count past the buffer"))?;
                    self.bytes.extend_from_slice(kept);
                    self.truncated |= accepted != count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// Read everything the pipe holds now, without waiting for more.
    pub(crate) fn drain(&mut self) -> io::Result<()> {
        while self.poll()? {}
        Ok(())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// The pipe to wake on: open and under its cap. Output past the cap
    /// drains at the loop's pace, so a flood cannot spin it.
    pub(crate) fn awaited(&self) -> Option<BorrowedFd<'_>> {
        (!self.closed && !self.truncated).then(|| self.reader.as_fd())
    }
}

/// Sleep up to `timeout`, waking early when one of `streams` has bytes or
/// closes. A pipe then moves at its writer's pace instead of one pipe buffer
/// per sleep.
pub(crate) fn wait_readable(streams: &[Option<BorrowedFd<'_>>], timeout: Duration) {
    let mut set: Vec<PollFd<'_>> = streams
        .iter()
        .flatten()
        .map(|stream| PollFd::from_borrowed_fd(*stream, PollFlags::IN))
        .collect();
    // Readiness only paces the loop; reads report their own errors. Without a
    // usable wait, sleep.
    let waited = !set.is_empty()
        && Timespec::try_from(timeout).is_ok_and(|limit| rustix::event::poll(&mut set, Some(&limit)).is_ok());
    if !waited {
        thread::sleep(timeout);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn shell(script: &str) -> impl FnOnce(&Path) -> io::Result<Program> {
        let script = script.to_owned();
        move |inputs| {
            Ok(Program {
                command: vec!["sh".into(), "-c".into(), script.into()],
                environment: vec![("RUN_INPUTS".into(), inputs.as_os_str().to_owned())],
                cwd: inputs.to_path_buf(),
                max_stdout_bytes: 64,
                stdin: Some(b"from stdin".to_vec()),
            })
        }
    }

    fn failure(result: Result<Exit, Failure>) -> Result<Failure, Box<dyn std::error::Error>> {
        match result {
            Ok(exit) => Err(format!("the run succeeded: {exit:?}").into()),
            Err(failure) => Ok(failure),
        }
    }

    #[test]
    fn a_run_returns_its_exit_code_and_output() -> TestResult {
        let exit = run(
            Duration::from_secs(10),
            || false,
            Box::new(()),
            shell("cat; echo oops >&2; exit 3"),
        )
        .map_err(|failure| failure.detail)?;
        assert_eq!(exit.code, 3);
        assert_eq!(exit.stdout, b"from stdin");
        assert_eq!(exit.stderr, "oops\n");
        Ok(())
    }

    #[test]
    fn a_cancelled_run_writes_no_inputs_and_never_starts() -> TestResult {
        let prepared = Cell::new(false);
        let failure = failure(run(
            Duration::from_secs(10),
            || true,
            Box::new(()),
            |_| {
                prepared.set(true);
                Err(io::Error::other("unreachable"))
            },
        ))?;
        assert_eq!(failure.disposition, Disposition::NotStarted);
        assert!(!prepared.get());
        Ok(())
    }

    #[test]
    fn output_past_the_cap_stops_the_group() -> TestResult {
        let started = Instant::now();
        let failure = failure(run(
            Duration::from_secs(30),
            || false,
            Box::new(()),
            shell("while :; do echo 0123456789; done"),
        ))?;
        assert_eq!(failure.disposition, Disposition::Stopped, "{}", failure.detail);
        assert!(failure.detail.contains("more than 64 bytes"), "{}", failure.detail);
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    #[test]
    fn a_run_past_its_limit_stops_the_group() -> TestResult {
        let started = Instant::now();
        let failure = failure(run(
            Duration::from_millis(300),
            || false,
            Box::new(()),
            shell("sleep 30"),
        ))?;
        assert_eq!(failure.disposition, Disposition::Stopped, "{}", failure.detail);
        assert!(failure.detail.contains("limit"), "{}", failure.detail);
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    /// A background process keeps the stdout pipe open after the leader
    /// exits. The reply must not wait for end of file.
    #[test]
    fn a_leftover_member_holding_stdout_open_is_stopped_without_waiting_for_eof() -> TestResult {
        let started = Instant::now();
        let failure = failure(run(
            Duration::from_secs(30),
            || false,
            Box::new(()),
            shell("sleep 30 & echo done"),
        ))?;
        assert_eq!(failure.disposition, Disposition::Stopped, "{}", failure.detail);
        assert!(failure.detail.contains("left processes"), "{}", failure.detail);
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }
}
