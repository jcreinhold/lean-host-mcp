//! Builds: Lake runs in a project under the project's write turn.
//!
//! Requirement: a build runs alone. It first takes the root's write turn, so
//! it waits for the checks that hold read turns and holds back the checks that
//! come after it. Then it stops the root's workers and waits until each has
//! exited, so none still holds an old build's files; the next check starts a
//! fresh worker. A build that changes nothing stops them too. Then it runs the
//! guard and each step as `lake <args>` in the Lake root, each step in its own
//! process group with its own child file (`children.rs`). Each step's
//! output goes to one log file under the store, capped at `[build]
//! log_limit_mib`, and the reply carries the log's tail. Cancellation or the
//! build's deadline stops the running step's whole group. The turn ends only
//! after each step's group has exited, so no check starts while a process of
//! the build still runs.
//!
//! Lake decides what to build and in which order. The server checks only the
//! words a client adds (see `lake_args`) and the config that names the rest
//! (see `config`).

pub(crate) mod config;
mod lake_args;
mod log;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use lean_toolchain::process_group::{OwnedCommandGroup, group_present};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::broker::ProjectBroker;
use crate::cancellation::{CallCancellation, with_deadline};
use crate::children::ChildFile;
use crate::error::{Result, ServerError};
use crate::project_lock::{TurnRefused, WriteTurn, ensure_write};
use config::{Append, BuildConfig, GuardPlace};
use log::{CappedLog, LogSink};

/// How long a step's loop waits for output before it checks the group again.
const POLL: Duration = Duration::from_millis(25);
/// How long Lake may leave other members of its group running after it exits
/// before the build stops them.
const LEFTOVER_GRACE: Duration = Duration::from_secs(1);
/// How often a build warns while it waits for a stopped step's group.
const EXIT_WARNING: Duration = Duration::from_secs(30);

/// `lean_build` arguments.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRequest {
    /// Lake root. Defaults to the server's project.
    #[serde(default)]
    pub project: Option<String>,
    /// Lake targets to build. Exclusive with `recipe`. With neither, Lake builds the default targets.
    #[serde(default)]
    pub targets: Vec<String>,
    /// A recipe from the project's `[recipes]` config.
    #[serde(default)]
    pub recipe: Option<String>,
    /// Words the recipe appends to its steps, where its `append` allows. A word may not start with `-`.
    #[serde(default)]
    pub args: Vec<String>,
}

/// How a build ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    /// Every step that should run ran. `ok` says whether all passed.
    Done,
    /// The deadline stopped the build.
    TimedOut,
}

/// One Lake run of a build.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StepOutcome {
    /// The full command, starting with `lake`.
    pub argv: Vec<String>,
    /// Lake's exit code, or 128 plus the signal that ended it. Null when the
    /// step was stopped or its result was lost.
    pub exit_code: Option<i32>,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
}

/// The build's log file.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BuildLog {
    pub path: String,
    pub bytes: u64,
    /// Whether output past `[build] log_limit_mib` was dropped.
    pub truncated: bool,
}

/// What a build did.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct BuildOutcome {
    pub state: BuildState,
    /// Whether every step ran and passed.
    pub ok: bool,
    /// The first failing step's exit code, 0 when all passed, or null when
    /// the build was stopped.
    pub exit_code: Option<i32>,
    /// Index in `steps` of the first step that failed.
    pub failed_step: Option<usize>,
    pub steps: Vec<StepOutcome>,
    pub log: BuildLog,
    /// The end of the log.
    pub tail: String,
}

/// How many bytes of the log's end a reply carries, unless `[output]
/// max_field_bytes` is smaller.
const TAIL_BYTES: usize = 8 << 10;

/// What one build runs, in order, and its limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildPlan {
    /// The words after `lake` of each run. The first run that fails ends the
    /// build.
    pub(crate) steps: Vec<Vec<String>>,
    pub(crate) timeout: Duration,
    /// The log's cap in bytes.
    pub(crate) log_limit: u64,
    /// How many bytes of the log's end the reply carries.
    pub(crate) tail_bytes: usize,
}

impl BuildPlan {
    /// Carry no more of the log's end than `max_field_bytes`, when the
    /// server's `[output]` config sets it.
    pub(crate) fn cap_tail(&mut self, max_field_bytes: Option<u32>) {
        if let Some(cap) = max_field_bytes.and_then(|cap| usize::try_from(cap).ok()) {
            self.tail_bytes = self.tail_bytes.min(cap);
        }
    }
}

/// Plan the build `request` asks for under the project's `config`.
///
/// # Errors
/// Names what the request or the config gets wrong.
pub(crate) fn plan(config: &BuildConfig, request: &BuildRequest) -> std::result::Result<BuildPlan, String> {
    let settings = &config.build;
    let guard = (!settings.guard.is_empty()).then(|| {
        ["build", "--no-build"]
            .into_iter()
            .map(str::to_owned)
            .chain(settings.guard.iter().cloned())
            .collect::<Vec<_>>()
    });
    let mut steps = Vec::new();
    let mut timeout_secs = settings.timeout_secs;
    if let Some(name) = request.recipe.as_deref() {
        if !request.targets.is_empty() {
            return Err("`targets` and `recipe` exclude each other".to_owned());
        }
        let Some(recipe) = config.recipes.get(name) else {
            let known = config.recipes.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
            return Err(format!(
                "no recipe `{name}`; this project has {}",
                if known.is_empty() { "none" } else { &known }
            ));
        };
        timeout_secs = recipe.timeout_secs.unwrap_or(timeout_secs);
        if recipe.append == Append::None && !request.args.is_empty() {
            return Err(format!("recipe `{name}` takes no `args`"));
        }
        lake_args::check_client_words(&request.args).map_err(|problem| format!("recipe `{name}`: {problem}"))?;
        let (before, after) = match recipe.guard {
            GuardPlace::Before => (guard, None),
            GuardPlace::After => (None, guard),
            GuardPlace::None => (None, None),
        };
        steps.extend(before);
        let last = recipe.steps.len().saturating_sub(1);
        for (index, words) in recipe.steps.iter().enumerate() {
            let appends = match recipe.append {
                Append::None => false,
                Append::Last => index == last,
                Append::Each => true,
            };
            let mut args = words.clone();
            if appends {
                args.extend(request.args.iter().cloned());
            }
            steps.push(args);
        }
        steps.extend(after);
    } else {
        if !request.args.is_empty() {
            return Err("`args` needs a `recipe`; name Lake targets in `targets`".to_owned());
        }
        lake_args::check_client_words(&request.targets)?;
        let mut args = vec!["build".to_owned()];
        args.extend(request.targets.iter().cloned());
        steps.extend(guard);
        steps.push(args);
    }
    Ok(BuildPlan {
        steps,
        timeout: Duration::from_secs(timeout_secs),
        log_limit: settings.log_limit_mib.saturating_mul(1 << 20),
        tail_bytes: TAIL_BYTES,
    })
}

/// A new build id: `srv-` and a random UUID.
pub(crate) fn new_id() -> String {
    format!("srv-{}", uuid::Uuid::new_v4())
}

/// The log file of build `id` in server store `store`, as an absolute path.
/// Creates the store's `logs` directory with mode 0700.
///
/// # Errors
/// Returns the error from creating or resolving the directory.
pub(crate) fn log_path(store: &Path, id: &str) -> io::Result<PathBuf> {
    let logs = store.join("logs");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&logs)?;
    let mut name = crate::digest::hex(Sha256::digest(id.as_bytes()).into_iter().take(16));
    name.push_str(".log");
    Ok(logs.canonicalize()?.join(name))
}

/// Work a build does once it holds the write turn and its workers have
/// exited, before its first step: a request records its row as `running`. An
/// error ends the build before any step starts.
pub(crate) type OnRunning = Box<dyn FnOnce() -> std::result::Result<(), String> + Send>;

/// Run `plan` in canonical Lake root `root` as build `id`, writing its output
/// to `log`. The plan's timeout bounds the wait for the turn, the workers'
/// exit and the run together. `on_running` runs after the workers have
/// exited and before the first step.
///
/// # Errors
/// Fails when the build gets no turn, when a worker of `root` does not exit,
/// when `on_running` fails, or when its request is cancelled. A failing step
/// is not an error.
pub(crate) async fn run_build(
    broker: &ProjectBroker,
    root: &Path,
    plan: BuildPlan,
    id: &str,
    log: &Path,
    on_running: Option<OnRunning>,
) -> Result<BuildOutcome> {
    let timeout = plan.timeout;
    with_deadline(timeout, async move {
        let cancellation = CallCancellation::current();
        let deadline = cancellation
            .deadline_or(timeout)
            .ok_or_else(|| ServerError::Internal("build deadline is out of range".to_owned()))?;
        let sysroot = crate::toolchain::ToolchainId::from_lake_root(root)
            .and_then(|toolchain| toolchain.elan_dir())
            .map_err(|error| ServerError::BadProject(error.to_string()))?
            .canonicalize()?;
        let mut capped = CappedLog::open(log, plan.log_limit)?;
        capped.append(format!("[lean-host-mcp: build {id} in {}]\n", root.display()).as_bytes())?;
        let turn = ensure_write(&broker.project_lock(root), id, deadline, &cancellation)
            .await
            .map_err(|refused| ProjectBroker::turn_refused(root, refused))?;
        broker.stop_workers(root, deadline).await?;
        let job = Job {
            root: root.to_path_buf(),
            sysroot,
            log: log.to_path_buf(),
            capped,
            deadline: deadline.into_std(),
            cancellation,
            plan,
            on_running,
        };
        // The job owns the turn, so a dropped request cannot end the turn
        // before the last step's group has exited; the drop cancels the job
        // instead.
        tokio::task::spawn_blocking(move || job.run(turn))
            .await
            .map_err(|error| ServerError::Internal(format!("build task failed: {error}")))?
    })
    .await
}

/// Why a build stopped before its plan ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Deadline,
    /// A step could not start, or the server lost track of its group.
    Failed,
}

/// How one step ended.
enum StepEnd {
    /// Lake exited with this status, and its group has exited.
    Exited(ExitStatus),
    /// The server stopped the step's group because the build was cancelled
    /// or ran out of time; the group has exited.
    Stopped,
    /// Lake exited but left other members of its group running, which the
    /// server stopped; the group has exited. The step has no result, since
    /// the work it started did not finish.
    LeftRunning,
    /// The step did not start.
    NotStarted(String),
    /// The server lost track of the step's group, which has since gone or
    /// can no longer be watched.
    Lost(String),
}

/// A build with its turn granted, run on a blocking thread.
struct Job {
    root: PathBuf,
    sysroot: PathBuf,
    log: PathBuf,
    capped: CappedLog,
    deadline: Instant,
    cancellation: CallCancellation,
    plan: BuildPlan,
    on_running: Option<OnRunning>,
}

impl Job {
    fn stop_requested(&self) -> bool {
        self.cancellation.is_cancelled() || Instant::now() >= self.deadline
    }

    fn why_stopped(&self) -> Stop {
        if self.cancellation.is_cancelled() {
            Stop::Cancelled
        } else if Instant::now() >= self.deadline {
            Stop::Deadline
        } else {
            Stop::Failed
        }
    }

    /// Add a line of the server's own to the log. A failure here never stops
    /// the build.
    fn note(&mut self, line: &str) {
        if let Err(error) = self.capped.append(line.as_bytes()) {
            tracing::warn!(log = %self.log.display(), %error, "could not write to the build log");
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(self.sysroot.join("bin").join("lake"));
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .env("LEAN_SYSROOT", &self.sysroot)
            // Keep build products under each package's `.lake/build`, where
            // setup-file and the workers read them.
            .env("LAKE_ARTIFACT_CACHE", "false");
        crate::toolchain::pass_elan_home(&mut command);
        command
    }

    /// Run `lake <args>` in its own group, copying its output into the log,
    /// and return once the group has exited.
    fn step(&mut self, args: &[String]) -> StepEnd {
        let mut command = self.command(args);
        let (reader, writer) = match io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => return StepEnd::NotStarted(format!("cannot make a pipe for Lake's output: {error}")),
        };
        let stderr = match writer.try_clone() {
            Ok(stderr) => stderr,
            Err(error) => return StepEnd::NotStarted(format!("cannot make a pipe for Lake's output: {error}")),
        };
        command.stdout(writer).stderr(stderr);
        let mut sink = match LogSink::new(reader) {
            Ok(sink) => sink,
            Err(error) => return StepEnd::NotStarted(format!("cannot read Lake's output: {error}")),
        };
        let elsewhere = self.log.parent().unwrap_or(&self.root).to_path_buf();
        let mut child_file = match ChildFile::create(&elsewhere) {
            Ok(file) => file,
            Err(error) => return StepEnd::NotStarted(format!("cannot record Lake: {error}")),
        };
        let inherited: Vec<BorrowedFd<'_>> = child_file.as_fd().into_iter().collect();
        // The command, and with it this process's write ends of the pipe,
        // goes away here; only Lake holds them now.
        let spawned = OwnedCommandGroup::spawn_inheriting(command, &inherited);
        drop(inherited);
        let mut group = match spawned {
            Ok(group) => group,
            Err(error) => return StepEnd::NotStarted(format!("cannot start Lake: {error}")),
        };
        child_file.started(group.id());
        let mut stopped = false;
        let mut leftover: Option<Instant> = None;
        let mut stop_at: Option<Instant> = None;
        let mut warned_at: Option<Instant> = None;
        loop {
            sink.copy_into(&mut self.capped);
            match group.try_wait() {
                Ok(Some(status)) => {
                    // The group has exited: take what the pipe holds, then
                    // stop. A process outside the group may hold the pipe's
                    // write end, so never wait for its end.
                    sink.drain_into(&mut self.capped);
                    drop(child_file);
                    if stopped {
                        return StepEnd::Stopped;
                    }
                    if leftover.is_some() && stop_at.is_some() {
                        self.note(
                            "\n[lean-host-mcp: Lake left processes running in its group; the server stopped them]\n",
                        );
                        return StepEnd::LeftRunning;
                    }
                    return StepEnd::Exited(status);
                }
                Ok(None) => {}
                Err(error) => {
                    sink.drain_into(&mut self.capped);
                    return self.lost(group.id(), child_file, &error);
                }
            }
            if stop_at.is_none() {
                let requested = self.stop_requested();
                let left =
                    group.cleanup_pending() && leftover.get_or_insert_with(Instant::now).elapsed() >= LEFTOVER_GRACE;
                if requested || left {
                    stopped = requested;
                    if let Err(error) = group.request_stop() {
                        tracing::warn!(root = %self.root.display(), %error, "cannot stop a build step's process group; still trying");
                    }
                    stop_at = Some(Instant::now());
                }
            } else if let Some(at) = stop_at {
                // The SDK stops the group while Lake runs; this also stops
                // members Lake left behind.
                if let Err(error) = child_file.stop_group(group.id()) {
                    tracing::warn!(root = %self.root.display(), %error, "cannot stop the rest of a build step's process group");
                }
                if warned_at.unwrap_or(at).elapsed() >= EXIT_WARNING {
                    warned_at = Some(Instant::now());
                    tracing::warn!(
                        root = %self.root.display(),
                        group = group.id(),
                        waited_secs = at.elapsed().as_secs(),
                        "a stopped build step has not exited; the project's write turn stays held"
                    );
                }
            }
            crate::lean_run::wait_readable(&[sink.awaited()], POLL);
        }
    }

    /// The step whose group the SDK lost track of: watch the group until it
    /// has gone, signalling it only while its child file shows a member still
    /// runs. A group that cannot be watched keeps its child file, so the next
    /// server waits for it.
    fn lost(&mut self, group: u32, child_file: ChildFile, error: &io::Error) -> StepEnd {
        self.note(&format!(
            "\n[lean-host-mcp: lost track of Lake's process group {group}: {error}]\n"
        ));
        let mut warned = Instant::now();
        loop {
            match group_present(group) {
                Ok(false) => return StepEnd::Lost(format!("lost track of Lake's process group: {error}")),
                Ok(true) => {
                    if let Err(stop) = child_file.stop_group(group) {
                        tracing::warn!(root = %self.root.display(), group, %stop, "cannot stop a build step's process group");
                    }
                    if warned.elapsed() >= EXIT_WARNING {
                        warned = Instant::now();
                        tracing::warn!(root = %self.root.display(), group, "a build step's process group still runs; the project's write turn stays held");
                    }
                    std::thread::sleep(POLL);
                }
                Err(watch) => {
                    tracing::warn!(root = %self.root.display(), group, %watch, "cannot watch a build step's process group; the next server waits for it");
                    child_file.keep();
                    return StepEnd::Lost(format!(
                        "lost track of Lake's process group: {error}; cannot watch it: {watch}"
                    ));
                }
            }
        }
    }

    fn run(mut self, turn: WriteTurn) -> Result<BuildOutcome> {
        if let Some(Err(error)) = self.on_running.take().map(|record| record()) {
            drop(turn);
            return Err(ServerError::Internal(format!(
                "build did not start: cannot record it as running: {error}"
            )));
        }
        let mut steps = Vec::new();
        let mut failed_step = None;
        let mut stopped = None;
        let mut launched = false;
        for args in std::mem::take(&mut self.plan.steps) {
            if self.stop_requested() {
                stopped = Some(self.why_stopped());
                break;
            }
            let argv: Vec<String> = std::iter::once("lake".to_owned()).chain(args.iter().cloned()).collect();
            let shown: Vec<String> = argv.iter().map(|word| crate::toolchain::shell_word(word)).collect();
            self.note(&format!("\n$ {}\n", shown.join(" ")));
            let started_at_ms = epoch_millis();
            let exit_code = match self.step(&args) {
                StepEnd::Exited(status) => Some(status_code(status)),
                StepEnd::Stopped => {
                    stopped = Some(self.why_stopped());
                    launched = true;
                    None
                }
                StepEnd::LeftRunning => {
                    stopped = Some(Stop::Failed);
                    launched = true;
                    None
                }
                StepEnd::NotStarted(detail) => {
                    stopped = Some(self.why_stopped());
                    if self.stop_requested() {
                        break;
                    }
                    self.note(&format!("\n[lean-host-mcp: step did not start: {detail}]\n"));
                    None
                }
                StepEnd::Lost(detail) => {
                    stopped = Some(Stop::Failed);
                    launched = true;
                    self.note(&format!("\n[lean-host-mcp: step ended without a result: {detail}]\n"));
                    None
                }
            };
            launched |= exit_code.is_some();
            steps.push(StepOutcome {
                argv,
                exit_code,
                started_at_ms,
                finished_at_ms: epoch_millis(),
            });
            if exit_code != Some(0) {
                failed_step = Some(steps.len().saturating_sub(1));
                break;
            }
        }
        // Lake may have written build products once any step started.
        if launched {
            turn.finish();
        } else {
            drop(turn);
        }
        let (bytes, tail) = read_tail(&self.log, self.plan.tail_bytes)?;
        let log = BuildLog {
            path: self.log.display().to_string(),
            bytes,
            truncated: self.capped.truncated() || bytes > self.plan.log_limit,
        };
        let (state, ok, exit_code) = match stopped {
            Some(Stop::Cancelled) => {
                return Err(ProjectBroker::turn_refused(&self.root, TurnRefused::Cancelled));
            }
            Some(Stop::Deadline) => (BuildState::TimedOut, false, None),
            Some(Stop::Failed) => (BuildState::Done, false, None),
            None => {
                let code = failed_step
                    .and_then(|index| steps.get(index))
                    .map_or(Some(0), |step: &StepOutcome| step.exit_code);
                (BuildState::Done, failed_step.is_none(), code)
            }
        };
        Ok(BuildOutcome {
            state,
            ok,
            exit_code,
            failed_step,
            steps,
            log,
            tail,
        })
    }
}

/// Lake's exit code, or 128 plus the signal that ended it.
fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| status.signal().map_or(-1, |signal| signal.saturating_add(128)))
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// The log's size and its last `tail_bytes` bytes, starting at a character.
fn read_tail(path: &Path, tail_bytes: usize) -> io::Result<(u64, String)> {
    use rustix::fs::{Mode, OFlags};
    let mut file = File::from(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let bytes = file.metadata()?.len();
    let want = u64::try_from(tail_bytes).unwrap_or(u64::MAX).min(bytes);
    file.seek(SeekFrom::Start(bytes.saturating_sub(want)))?;
    let mut buffer = Vec::new();
    file.take(want).read_to_end(&mut buffer)?;
    // Skip the rest of a character the cut split.
    let start = buffer.iter().take(3).take_while(|byte| **byte & 0xC0 == 0x80).count();
    let text = String::from_utf8_lossy(buffer.get(start..).unwrap_or_default()).into_owned();
    Ok((bytes, text))
}
