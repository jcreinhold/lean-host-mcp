//! Reader runs: short runs a request starts to read the project, such as
//! `lake setup-file`, `lake query` or a file check. Each holds a permit from
//! `broker.max_lean_runs` and a share of the request's read turn until its
//! process group has exited, so no build starts while it reads.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cancellation::CallCancellation;
use crate::error::ServerError;
use crate::lean_run::{self, Disposition};

/// A failed reader run, with what became of its process.
#[derive(Debug)]
pub(crate) struct PreparationFailure {
    pub(crate) error: ServerError,
    disposition: Disposition,
}

type PreparationResult<T> = std::result::Result<T, PreparationFailure>;

impl PreparationFailure {
    fn stopped(error: ServerError) -> Self {
        Self {
            error,
            disposition: Disposition::Stopped,
        }
    }

    fn unknown(error: ServerError) -> Self {
        Self {
            error,
            disposition: Disposition::Unknown,
        }
    }
}

impl From<ServerError> for PreparationFailure {
    fn from(error: ServerError) -> Self {
        // These conversions precede the run, so nothing started. Failures
        // after the start carry the run's own disposition.
        Self {
            error,
            disposition: Disposition::NotStarted,
        }
    }
}

/// Run one short program for a request on canonical root `root` and return
/// its stdout, when it exits 0. Cancellation stops the run rather than
/// dropping its process evidence. One deadline covers the wait for a permit
/// from `runs` and the run.
pub(crate) async fn reader_output(
    label: &'static str,
    root: &Path,
    timeout: Duration,
    runs: Arc<Semaphore>,
    keepalive: Option<Box<dyn Send>>,
    prepare: impl FnOnce(&Path) -> std::io::Result<lean_run::Program> + Send + 'static,
) -> PreparationResult<Vec<u8>> {
    let output = reader_run(label, root, timeout, runs, keepalive, &[0], prepare).await?;
    Ok(output.stdout)
}

/// How a short run's program ended, when it ended with an exit code the
/// caller accepts.
#[derive(Debug)]
pub(crate) struct ReaderOutput {
    pub(crate) exit_code: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

impl ReaderOutput {
    /// The error for a run of `label` that ended with this exit code when its
    /// caller wanted another: the code and everything the program printed.
    pub(crate) fn exit_error(&self, label: &str) -> ServerError {
        reader_error(
            label,
            format!(
                "the program exited with code {}: {}{}",
                self.exit_code,
                String::from_utf8_lossy(&self.stdout),
                self.stderr
            ),
        )
    }
}

/// [`reader_output`] for a program whose exit codes in `accepted` all carry a
/// result, such as `lean`, which exits 1 when the file it checks has errors.
///
/// The run holds a permit from `runs` and a share of the request's read turn
/// on `root` until its process group has exited, even past the reply.
pub(crate) async fn reader_run(
    label: &'static str,
    root: &Path,
    timeout: Duration,
    runs: Arc<Semaphore>,
    keepalive: Option<Box<dyn Send>>,
    accepted: &'static [i32],
    prepare: impl FnOnce(&Path) -> std::io::Result<lean_run::Program> + Send + 'static,
) -> PreparationResult<ReaderOutput> {
    let cancellation = CallCancellation::current();
    let mut on_drop = cancellation.cancel_on_drop();
    let started = Instant::now();
    let permit = match run_permit(&cancellation, runs, timeout).await {
        Ok(permit) => permit,
        Err(detail) => {
            cancellation.begin_operation().not_started();
            on_drop.disarm();
            return Err(reader_error(label, detail).into());
        }
    };
    let operation = cancellation.begin_operation();
    let share = crate::project_lock::read_share(root).await;
    let remaining = timeout.saturating_sub(started.elapsed());
    let result = tokio::task::spawn_blocking(move || {
        let keepalive: Box<dyn Send> = Box::new((permit, share, keepalive));
        let result = lean_run::run(remaining, || cancellation.is_cancelled(), keepalive, prepare)
            .map_err(|failure| PreparationFailure {
                disposition: failure.disposition,
                error: reader_error(label, failure.detail),
            })
            .and_then(|exit| accepted_exit(label, exit, accepted));
        match result
            .as_ref()
            .map_or_else(|failure| failure.disposition, |_| Disposition::Stopped)
        {
            Disposition::NotStarted => operation.not_started(),
            Disposition::Stopped => operation.returned(),
            Disposition::Unknown => operation.unknown(),
        }
        result
    })
    .await;
    on_drop.disarm();
    result.map_err(|error| PreparationFailure::unknown(reader_error(label, error)))?
}

/// Wait up to `timeout` for a permit from `runs`, the pool's bound on short
/// runs, or say why none came.
async fn run_permit(
    cancellation: &CallCancellation,
    runs: Arc<Semaphore>,
    timeout: Duration,
) -> std::result::Result<OwnedSemaphorePermit, String> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err("cancelled before a short-run permit came free".to_owned()),
        permit = tokio::time::timeout(timeout, runs.acquire_owned()) => match permit {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(closed)) => Err(format!("the short-run permits closed: {closed}")),
            Err(_) => Err(format!(
                "no short-run permit came free within {} s; raise broker.max_lean_runs or retry",
                timeout.as_secs_f64()
            )),
        },
    }
}

/// The output of a program that exited on its own, if `accepted` lists its
/// exit code.
fn accepted_exit(label: &str, exit: lean_run::Exit, accepted: &[i32]) -> PreparationResult<ReaderOutput> {
    let output = ReaderOutput {
        exit_code: exit.code,
        stdout: exit.stdout,
        stderr: exit.stderr,
    };
    if accepted.contains(&output.exit_code) {
        return Ok(output);
    }
    Err(PreparationFailure::stopped(output.exit_error(label)))
}

fn reader_error(label: &str, error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(format!("{label}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn exit(code: i32, stderr: &str) -> lean_run::Exit {
        lean_run::Exit {
            code,
            stdout: b"{}".to_vec(),
            stderr: stderr.to_owned(),
        }
    }

    /// A program for a run that must never start: it records that it was
    /// asked for in `asked`.
    fn never(asked: &Arc<AtomicBool>) -> impl FnOnce(&Path) -> std::io::Result<lean_run::Program> + Send + 'static {
        let asked = Arc::clone(asked);
        move |_| {
            asked.store(true, Ordering::SeqCst);
            Err(std::io::Error::other("this run must never start"))
        }
    }

    #[test]
    fn a_run_that_exits_with_a_code_its_caller_refuses_fails_with_the_output() -> TestResult {
        assert!(accepted_exit("Lake setup-file", exit(0, ""), &[0]).is_ok());
        let Err(failure) = accepted_exit("Lake setup-file", exit(1, "why"), &[0]) else {
            return Err("exit 1 passed".into());
        };
        assert_eq!(failure.disposition, Disposition::Stopped);
        let message = failure.error.to_string();
        assert!(
            message.contains("Lake setup-file: the program exited with code 1"),
            "{message}"
        );
        assert!(message.contains("why"), "{message}");
        Ok(())
    }

    #[test]
    fn a_caller_that_accepts_an_exit_code_gets_it_with_the_output() -> TestResult {
        let output =
            accepted_exit("Lean file diagnostics", exit(1, "why"), &[0, 1]).map_err(|failure| failure.error)?;
        assert_eq!((output.exit_code, output.stderr.as_str()), (1, "why"));
        assert!(accepted_exit("Lean file diagnostics", exit(2, ""), &[0, 1]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn a_short_run_reports_its_own_error_and_disposition() -> TestResult {
        for (script, disposition, detail) in [
            ("exit 4", Disposition::Stopped, "exited with code 4"),
            ("sleep 30", Disposition::Stopped, "limit"),
        ] {
            let result = reader_output(
                "Lean file driver",
                Path::new("/"),
                Duration::from_millis(300),
                Arc::new(Semaphore::new(1)),
                None,
                move |inputs| {
                    Ok(lean_run::Program {
                        command: vec!["sh".into(), "-c".into(), script.into()],
                        environment: Vec::new(),
                        cwd: inputs.to_path_buf(),
                        max_stdout_bytes: lean_run::OUTPUT_LIMIT,
                        stdin: None,
                    })
                },
            )
            .await;
            let Err(failure) = result else {
                return Err(format!("`{script}` passed").into());
            };
            assert_eq!(failure.disposition, disposition);
            let error = rmcp::ErrorData::from(failure.error);
            assert!(
                error.message.starts_with("lake project not usable: Lean file driver: "),
                "{error:?}"
            );
            assert!(error.message.contains(detail), "{error:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn pre_cancelled_reader_admission_records_no_start_without_preparing_inputs() -> TestResult {
        let root = tempfile::tempdir()?;
        let request = tokio_util::sync::CancellationToken::new();
        request.cancel();
        let asked = Arc::new(AtomicBool::new(false));
        let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            reader_output(
                "Lake setup-file",
                root.path(),
                Duration::from_secs(1),
                Arc::new(Semaphore::new(1)),
                None,
                never(&asked),
            )
            .await
        })
        .await;
        assert!(matches!(result, Err(failure) if failure.disposition == Disposition::NotStarted));
        assert!(!asked.load(Ordering::SeqCst), "a cancelled run prepared its inputs");
        assert_eq!(evidence.not_started, 1);
        assert_eq!(evidence.pending, 0);
        assert!(!evidence.uncertain);
        Ok(())
    }

    #[tokio::test]
    async fn a_run_that_gets_no_permit_by_its_deadline_never_starts() -> TestResult {
        let root = tempfile::tempdir()?;
        let runs = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&runs).acquire_owned().await?;
        let request = tokio_util::sync::CancellationToken::new();
        let asked = Arc::new(AtomicBool::new(false));
        let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            reader_output(
                "Lean file driver",
                root.path(),
                Duration::from_millis(50),
                runs,
                None,
                never(&asked),
            )
            .await
        })
        .await;
        let Err(failure) = result else {
            return Err("a run without a permit passed".into());
        };
        assert_eq!(failure.disposition, Disposition::NotStarted);
        assert!(failure.error.to_string().contains("broker.max_lean_runs"));
        assert!(
            !asked.load(Ordering::SeqCst),
            "a run without a permit prepared its inputs"
        );
        assert_eq!(evidence.not_started, 1);
        assert!(!evidence.uncertain);
        Ok(())
    }
}
