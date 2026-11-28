//! Resolve compilation options through the pinned Lake API, without building imports.
//!
//! Resolve before looking up a query cache. Configuration can run arbitrary Lean code,
//! including reading other files or environment variables, so we do not pretend that
//! hashing only `lakefile.lean` is sufficient to cache its evaluated options.

use std::collections::BTreeMap;
#[cfg(test)]
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::LeanWorkerElabOptions;
use lean_toolchain::LeanOptionValue;
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{Result, ServerError};
use crate::lean_run::{self, Disposition};
use crate::toolchain::ToolchainId;

const HELPER: &str = include_str!("lake/ModuleOptions.lean");
const LAKE_OPTIONS: &str = "Lake option discovery";
/// Option discovery's bound outside a request; a request uses its own deadline.
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct ModuleOptions {
    module: String,
    options: BTreeMap<String, LeanOptionValue>,
}

/// Preserve process evidence until the preparation operation records its outcome.
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
        // These conversions precede the run: metadata reads and private inputs
        // cannot have evaluated Lake configuration. Failures after the start
        // carry the run's own disposition.
        Self {
            error,
            disposition: Disposition::NotStarted,
        }
    }
}

/// Evaluate the owning package, then carry its options through every
/// worker and cache path. The evaluation is a short run and takes one of
/// `runs`.
pub(crate) async fn resolve(
    root: &Path,
    options: LeanWorkerElabOptions,
    runs: Arc<Semaphore>,
    keepalive: Option<Box<dyn Send>>,
) -> Result<LeanWorkerElabOptions> {
    let file_label = options.file_label.clone();
    resolve_inner(root, options, runs, keepalive)
        .await
        .map_err(|failure| preparation_failed(file_label, failure.error))
}

#[cfg(test)]
async fn observe_preparation(
    file_label: String,
    preparation: impl Future<Output = PreparationResult<LeanWorkerElabOptions>>,
) -> Result<LeanWorkerElabOptions> {
    let operation = CallCancellation::current().begin_operation();
    match preparation.await {
        Ok(options) => {
            operation.returned();
            Ok(options)
        }
        Err(PreparationFailure { error, disposition }) => {
            match disposition {
                Disposition::NotStarted => operation.not_started(),
                // Confirmed closure settles execution even though this failed
                // preparation supplies no option data or native-worker receipt.
                Disposition::Stopped => operation.returned(),
                Disposition::Unknown => operation.unknown(),
            }
            Err(preparation_failed(file_label, error))
        }
    }
}

fn preparation_failed(file_label: String, error: ServerError) -> ServerError {
    let target_check = TargetCheckNotStarted {
        file_label,
        stage: "lake_option_discovery".to_owned(),
        disposition: "not_started".to_owned(),
    };
    // Option discovery can execute Lake configuration and spawn subprocesses.
    // Its failure proves only that this target check was not reached.
    CallCancellation::current().target_check_not_started(target_check.clone());
    let message = if let ServerError::BadProject(message) = error {
        message
    } else {
        error.to_string()
    };
    ServerError::PreparationFailed { message, target_check }
}

async fn resolve_inner(
    root: &Path,
    options: LeanWorkerElabOptions,
    runs: Arc<Semaphore>,
    keepalive: Option<Box<dyn Send>>,
) -> PreparationResult<LeanWorkerElabOptions> {
    let preflight = CallCancellation::current().begin_operation();
    let context: PreparationResult<_> = (|| {
        let toolchain = ToolchainId::from_lake_root(root).map_err(bad_project)?;
        let sysroot = toolchain.elan_dir().map_err(bad_project)?;
        let file = source_path(root, &options.file_label);
        let (package, config_file, dependency_name) = owning_config(root, file.as_deref())?;
        let canonical_root = root.canonicalize().map_err(bad_project)?;
        Ok((sysroot, file, package, config_file, dependency_name, canonical_root))
    })();
    // Metadata reads have not started a configuration command.
    preflight.not_started();
    let (sysroot, file, package, config_file, dependency_name, root) = context?;
    let command_root = root.clone();
    let command_package = package.clone();
    let output = reader_output(
        LAKE_OPTIONS,
        &root,
        CallCancellation::current().remaining_or(TIMEOUT),
        runs,
        keepalive,
        move |assets| {
            // The run keeps these private inputs until its group has exited,
            // including after a lost or cancelled async waiter.
            let helper = assets.join("ModuleOptions.lean");
            std::fs::write(&helper, HELPER)?;
            let config_workspace = assets.join("config");
            std::fs::create_dir(&config_workspace)?;
            let plugin = sysroot.join("lib/lean").join(if cfg!(target_os = "macos") {
                "libLake_shared.dylib"
            } else {
                "libLake_shared.so"
            });
            let command = vec![
                sysroot.join("bin/lean").into_os_string(),
                "--plugin".into(),
                plugin.into_os_string(),
                "--run".into(),
                helper.into_os_string(),
                command_root.into_os_string(),
                command_package.clone().into_os_string(),
                config_file.into(),
                dependency_name.into(),
                config_workspace.into_os_string(),
                file.as_deref().unwrap_or_else(|| Path::new("")).as_os_str().to_owned(),
            ];
            Ok(lean_run::Program {
                command,
                environment: vec![("LEAN_SYSROOT".into(), sysroot.into_os_string())],
                cwd: command_package,
                max_stdout_bytes: lean_run::OUTPUT_LIMIT,
                stdin: None,
            })
        },
    )
    .await?;
    let resolved: ModuleOptions = serde_json::from_slice(&output).map_err(|error| {
        PreparationFailure::stopped(ServerError::BadProject(format!(
            "invalid Lake option metadata: {error}; no proof check ran"
        )))
    })?;
    tracing::debug!(
        package = %package.display(), module = %resolved.module,
        lean_options = ?resolved.options, heartbeat_limit = options.heartbeat_limit,
        "resolved Lake compilation options; host heartbeat limit overrides project maxHeartbeats"
    );
    Ok(options.lean_options(resolved.options))
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
            with_note(
                label,
                format!(
                    "the program exited with code {}: {}{}",
                    self.exit_code,
                    String::from_utf8_lossy(&self.stdout),
                    self.stderr
                ),
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
            return Err(reader_error(label, with_note(label, detail)).into());
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
                error: reader_error(label, with_note(label, failure.detail)),
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

/// `detail`, with the note that option discovery ran no proof check.
fn with_note(label: &str, detail: String) -> String {
    if label == LAKE_OPTIONS {
        format!("{detail}; no proof check ran")
    } else {
        detail
    }
}

#[cfg(test)]
async fn test_child_output(command: &mut tokio::process::Command, timeout: Duration) -> Result<std::process::Output> {
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| {
            ServerError::BadProject(format!(
                "Lake option discovery timed out after {} seconds; no proof check ran",
                timeout.as_secs_f64()
            ))
        })?
        .map_err(bad_project)
}

fn bad_project(error: impl std::fmt::Display) -> ServerError {
    reader_error(LAKE_OPTIONS, error)
}

fn reader_error(label: &str, error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(format!("{label}: {error}"))
}

fn source_path(root: &Path, label: &str) -> Option<PathBuf> {
    if label.starts_with('<') {
        None
    } else {
        Some(if Path::new(label).is_absolute() {
            PathBuf::from(label)
        } else {
            root.join(label)
        })
    }
}

/// Honor manifest package locations and custom config filenames before the
/// directory walk. This reads existing metadata only; it never materializes a dependency.
fn owning_config(root: &Path, file: Option<&Path>) -> Result<(PathBuf, String, String)> {
    if let Some(file) = file {
        let manifest_path = root.join("lake-manifest.json");
        if manifest_path.is_file() {
            let manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(manifest_path).map_err(bad_project)?).map_err(bad_project)?;
            let packages_dir = manifest
                .get("packagesDir")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(".lake/packages");
            let mut matched = None;
            if let Some(packages) = manifest.get("packages").and_then(serde_json::Value::as_array) {
                for entry in packages {
                    let name = entry
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim_matches(['«', '»']);
                    let mut package = if entry.get("type").and_then(serde_json::Value::as_str) == Some("path")
                        && entry.get("copy").and_then(serde_json::Value::as_bool) != Some(true)
                    {
                        root.join(
                            entry
                                .get("dir")
                                .and_then(serde_json::Value::as_str)
                                .ok_or_else(|| bad_project("path dependency missing dir"))?,
                        )
                    } else {
                        root.join(packages_dir).join(name)
                    };
                    if let Some(subdir) = entry.get("subDir").and_then(serde_json::Value::as_str) {
                        package.push(subdir);
                    }
                    let Ok(package) = package.canonicalize() else { continue };
                    if file.starts_with(&package)
                        && matched.as_ref().is_none_or(|(old, _, _): &(PathBuf, String, String)| {
                            package.components().count() > old.components().count()
                        })
                    {
                        let config = entry
                            .get("configFile")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("lakefile");
                        matched = Some((package, config.to_owned(), name.to_owned()));
                    }
                }
            }
            if let Some(matched) = matched {
                return Ok(matched);
            }
            if file.starts_with(root.join(packages_dir)) {
                return Err(bad_project(
                    "dependency source has no matching manifest package; cannot infer compilation options",
                ));
            }
        }
    }
    let package = owning_package(root, file);
    if package != root {
        return Err(bad_project(
            "dependency source has no matching manifest entry; cannot determine its Lake configuration options",
        ));
    }
    Ok((package, "lakefile".into(), String::new()))
}

/// Dependency files use their own package configuration, not the root library's.
/// Existing source resolution restricts files to the selected project. External
/// source files without a package use Lake's root-package external-file defaults.
fn owning_package(root: &Path, file: Option<&Path>) -> PathBuf {
    if let Some(file) = file {
        for parent in file.ancestors().skip(1) {
            if parent == root {
                break;
            }
            if parent.join("lakefile.lean").is_file() || parent.join("lakefile.toml").is_file() {
                return parent.to_path_buf();
            }
        }
    }
    root.to_path_buf()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn exit(code: i32, stderr: &str) -> lean_run::Exit {
        lean_run::Exit {
            code,
            stdout: b"{\"module\":\"Demo\",\"options\":{}}".to_vec(),
            stderr: stderr.to_owned(),
        }
    }

    #[test]
    fn discovery_accepts_only_exit_zero_and_says_no_check_ran() {
        assert!(accepted_exit(LAKE_OPTIONS, exit(0, ""), &[0]).is_ok());
        let error = accepted_exit(LAKE_OPTIONS, exit(1, "why"), &[0]).unwrap_err();
        assert_eq!(error.disposition, Disposition::Stopped);
        let message = error.error.to_string();
        assert!(message.contains("exited with code 1"), "{message}");
        assert!(message.contains("why"), "{message}");
        assert!(message.contains("no proof check ran"), "{message}");
    }

    #[test]
    fn a_caller_that_accepts_an_exit_code_gets_it_with_the_output() {
        let output = accepted_exit("Lean file diagnostics", exit(1, "why"), &[0, 1]).unwrap();
        assert_eq!((output.exit_code, output.stderr.as_str()), (1, "why"));
        assert!(accepted_exit("Lean file diagnostics", exit(2, ""), &[0, 1]).is_err());
    }

    #[tokio::test]
    async fn a_short_run_reports_its_own_error_and_disposition() {
        for (script, disposition, detail) in [
            ("exit 4", Disposition::Stopped, "exited with code 4"),
            ("sleep 30", Disposition::Stopped, "limit"),
        ] {
            let failure = reader_output(
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
            .await
            .unwrap_err();
            assert_eq!(failure.disposition, disposition);
            let error = rmcp::ErrorData::from(failure.error);
            assert!(
                error.message.starts_with("lake project not usable: Lean file driver: "),
                "{error:?}"
            );
            assert!(error.message.contains(detail), "{error:?}");
            assert!(!error.message.contains("no proof check ran"), "{error:?}");
        }
    }

    #[tokio::test]
    async fn completed_preparation_remains_started_when_the_target_is_never_dispatched() {
        let request = tokio_util::sync::CancellationToken::new();
        let ((), evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            observe_preparation("Demo.lean".to_owned(), async { Ok(LeanWorkerElabOptions::new()) })
                .await
                .unwrap();
            CallCancellation::current().begin_operation().not_started();
        })
        .await;
        assert_eq!(evidence.returned, 1);
        assert_eq!(evidence.not_started, 1);
        assert_eq!(evidence.pending, 0);
        assert!(!evidence.uncertain);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_child_timeout_only_certifies_the_named_check_was_not_started() {
        let request = tokio_util::sync::CancellationToken::new();
        let (error, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.arg("-c").arg("exec /bin/sleep 1").kill_on_drop(true);
            observe_preparation("UnitIndexed.lean".to_owned(), async {
                test_child_output(&mut command, Duration::from_millis(30))
                    .await
                    .map_err(PreparationFailure::unknown)?;
                Ok(LeanWorkerElabOptions::new())
            })
            .await
            .unwrap_err()
        })
        .await;
        assert!(evidence.uncertain, "kill_on_drop supplies no descendant exit receipt");
        let target = evidence.target_check.unwrap();
        assert_eq!(target.file_label, "UnitIndexed.lean");
        assert_eq!(target.disposition, "not_started");
        let error: rmcp::ErrorData = error.into();
        let data = error.data.unwrap();
        assert_eq!(data["target_check"]["stage"], "lake_option_discovery");
        assert!(
            data.get("execution").is_none(),
            "target evidence cannot certify the entire invocation"
        );
    }

    #[test]
    fn typed_metadata_preserves_option_types() {
        let metadata: ModuleOptions = serde_json::from_str(
            r#"{"module":"Demo","options":{"autoImplicit":false,"maxRecDepth":123,"pp.foo":"false"}}"#,
        )
        .unwrap();
        assert_eq!(metadata.options["autoImplicit"], LeanOptionValue::Bool(false));
        assert_eq!(metadata.options["maxRecDepth"], LeanOptionValue::Nat(123));
        assert_eq!(metadata.options["pp.foo"], LeanOptionValue::String("false".into()));
    }

    #[test]
    fn nested_package_does_not_inherit_root_options() {
        let temp = tempfile::tempdir().unwrap();
        let dep = temp.path().join(".lake/packages/dep");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("lakefile.toml"), "name = 'dep'").unwrap();
        assert_eq!(owning_package(temp.path(), Some(&dep.join("Dep.lean"))), dep);
        assert_eq!(owning_package(temp.path(), None), temp.path());
    }

    async fn fixture_resolve(root: &Path, label: &str) -> Result<LeanWorkerElabOptions> {
        resolve(
            root,
            LeanWorkerElabOptions::new().file_label(label),
            Arc::new(Semaphore::new(1)),
            None,
        )
        .await
    }

    fn stage_root(root: &Path, config: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join("lean-toolchain"),
            std::env::var("LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN").unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("lakefile.lean"), config).unwrap();
        std::fs::write(root.join("lake-manifest.json"), r#"{"version":"1.1.0","packages":[]}"#).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN"]
    async fn lake_module_options_follow_config_without_builds() {
        let temp = tempfile::Builder::new()
            .prefix("lake-options-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap().join("Project");
        let config = r#"import Lake
open Lake DSL
require dependency from ".lake/packages/dependency" with NameMap.empty.insert `strict "true"
package options_test where
  leanOptions := #[⟨`autoImplicit, (get_config? savedRoot).isNone⟩]
  moreLeanArgs := #["-D", "maxRecDepth=321", "-DautoImplicit=false", "--define=weak.testUnused=true"]
lean_lib Strict where
  leanOptions := #[⟨`autoImplicit, false⟩]
lean_lib Loose
"#;
        stage_root(&root, config);
        let dependency = root.join(".lake/packages/dependency");
        std::fs::create_dir_all(&dependency).unwrap();
        std::fs::write(
            dependency.join("lake-manifest.json"),
            r#"{"version":"1.1.0","packages":[]}"#,
        )
        .unwrap();
        std::fs::write(
            dependency.join("lakefile.toml"),
            "name = 'dependency'\n[leanOptions]\nautoImplicit = false\n[[lean_lib]]\nname = 'Dependency'\n",
        )
        .unwrap();
        let write_manifest = |config_file: &str| {
            std::fs::write(
                root.join("lake-manifest.json"),
                serde_json::to_vec(&serde_json::json!({
                    "version":"1.1.0", "packagesDir":".lake/packages", "packages":[{
                        "name":"dependency", "type":"path", "dir":".lake/packages/dependency",
                        "configFile":config_file, "manifestFile":"lake-manifest.json"
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
        };
        write_manifest("lakefile.toml");
        std::fs::write(root.join("Strict.lean"), "def sample := 1\n").unwrap();
        std::fs::write(root.join("Loose.lean"), "def sample := 1\n").unwrap();
        let resolved = fixture_resolve(&root, "Strict.lean").await.unwrap();
        assert_eq!(resolved.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert_eq!(resolved.lean_options["maxRecDepth"], LeanOptionValue::Nat(321));
        assert_eq!(
            resolved.lean_options["weak.testUnused"],
            LeanOptionValue::String("true".into())
        );
        let loose = tokio::time::timeout(Duration::from_secs(8), fixture_resolve(&root, "Loose.lean"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loose.lean_options["autoImplicit"], LeanOptionValue::Bool(true));
        assert!(!root.join(".lake/build").exists());
        assert!(!root.join(".lake/config").exists());
        let shared_root = root.join(".lake/config/0");
        std::fs::create_dir_all(&shared_root).unwrap();
        let saved = shared_root.join("lakefile.olean.trace");
        std::fs::write(&saved, r#"{"options":{"savedRoot":"true"}}"#).unwrap();
        let saved_bytes = std::fs::read(&saved).unwrap();
        let saved_modified = saved.metadata().unwrap().modified().unwrap();
        let loose = fixture_resolve(&root, "Loose.lean").await.unwrap();
        assert_eq!(loose.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert_eq!(std::fs::read(&saved).unwrap(), saved_bytes);
        assert_eq!(saved.metadata().unwrap().modified().unwrap(), saved_modified);
        let source = dependency.join("Dependency.lean");
        std::fs::write(&source, "def sample := 1\n").unwrap();
        let dep = fixture_resolve(&root, source.to_str().unwrap()).await.unwrap();
        assert_eq!(dep.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert!(!dep.lean_options.contains_key("maxRecDepth"));
        assert!(
            !dependency.join(".lake").exists(),
            "cold dependency gets no shared cache directory"
        );
        std::fs::rename(dependency.join("lakefile.toml"), dependency.join("custom.toml")).unwrap();
        write_manifest("custom.toml");
        assert_eq!(
            fixture_resolve(&root, source.to_str().unwrap())
                .await
                .unwrap()
                .lean_options["autoImplicit"],
            LeanOptionValue::Bool(false)
        );
        let shared_dep = dependency.join(".lake/config/0");
        std::fs::create_dir_all(&shared_dep).unwrap();
        std::fs::write(shared_dep.join("custom.olean"), "existing build config").unwrap();
        std::fs::write(shared_dep.join("custom.olean.trace"), "existing build trace").unwrap();
        std::fs::write(dependency.join("custom.lean"), "import Lake\nopen Lake DSL\npackage dependency\nlean_lib Dependency where\n  leanOptions := #[⟨`autoImplicit, (get_config? strict).isNone⟩]\n").unwrap();
        write_manifest("custom.lean");
        let dep = fixture_resolve(&root, source.to_str().unwrap()).await.unwrap();
        assert_eq!(
            dep.lean_options["autoImplicit"],
            LeanOptionValue::Bool(false),
            "parent require-with reaches dependency"
        );
        std::fs::write(
            root.join("lakefile.lean"),
            config.replace(" with NameMap.empty.insert `strict \"true\"", ""),
        )
        .unwrap();
        let dep = fixture_resolve(&root, source.to_str().unwrap()).await.unwrap();
        assert_eq!(
            dep.lean_options["autoImplicit"],
            LeanOptionValue::Bool(true),
            "fresh parent require-with is reevaluated"
        );
        assert_eq!(
            std::fs::read_to_string(shared_dep.join("custom.olean")).unwrap(),
            "existing build config"
        );
        assert_eq!(
            std::fs::read_to_string(shared_dep.join("custom.olean.trace")).unwrap(),
            "existing build trace"
        );
        assert!(!root.join(".lake/build").exists());
        assert!(!dependency.join(".lake/build").exists());
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN"]
    async fn cold_options_evaluate_external_inputs_fresh_with_real_package_paths() {
        let temp = tempfile::Builder::new()
            .prefix("lake-options-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap().join("Project");
        stage_root(
            &root,
            r#"import Lake
open Lean Lake DSL
run_cmd do
  let gate ← IO.FS.readFile (__dir__ / "gate")
  unless gate == "allowed" do throwError "fresh external file gate rejected"
  unless (← IO.currentDir) == __dir__ do throwError "real package cwd was not preserved"
package cold_options
lean_lib Demo
"#,
        );
        std::fs::write(root.join("gate"), "allowed").unwrap();
        std::fs::write(root.join("Demo.lean"), "def sample := 1\n").unwrap();
        fixture_resolve(&root, "Demo.lean").await.unwrap();
        assert!(
            !root.join(".lake").exists(),
            "cold root configuration never creates project .lake"
        );
        std::fs::write(root.join("gate"), "blocked").unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        let (result, evidence) =
            crate::cancellation::with_cancellation_evidence(&request, fixture_resolve(&root, "Demo.lean")).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fresh external file gate rejected")
        );
        assert_eq!(evidence.returned, 1, "failed configuration scope was confirmed closed");
        assert!(!evidence.uncertain);
        assert_eq!(evidence.target_check.unwrap().disposition, "not_started");
        assert!(!root.join(".lake").exists());
        std::fs::write(root.join("gate"), "allowed").unwrap();
        fixture_resolve(&root, "Demo.lean").await.unwrap();
    }

    async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(8), async {
            while !path.is_file() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN"]
    async fn configuration_cancellation_confirms_closed_scope_without_claiming_a_proof_result() {
        let temp = tempfile::Builder::new()
            .prefix("lake-options-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap().join("Project");
        stage_root(
            &root,
            r#"import Lake
open Lean Lake DSL
run_cmd do
  IO.FS.writeFile (__dir__ / "configuration-started") "started"
  while !(← (__dir__ / "release").pathExists) do IO.sleep 10
package cancelled_options
lean_lib Demo
"#,
        );
        std::fs::write(root.join("Demo.lean"), "def sample := 1\n").unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        let operation = crate::cancellation::with_cancellation_evidence(&request, fixture_resolve(&root, "Demo.lean"));
        tokio::pin!(operation);
        let started = root.join("configuration-started");
        tokio::select! {
            result = &mut operation => panic!("configuration ended before cancellation: {result:?}"),
            () = wait_for_file(&started) => {}
        }
        request.cancel();
        let (result, evidence) = tokio::time::timeout(Duration::from_secs(8), operation).await.unwrap();
        assert!(result.is_err());
        assert_eq!(evidence.returned, 1);
        assert_eq!(evidence.pending, 0);
        assert!(!evidence.uncertain, "owned cancellation supplies actual scope closure");
        assert!(
            evidence.worker_stop_receipt.is_none(),
            "no Lean worker proof result or receipt is fabricated"
        );
        assert_eq!(evidence.target_check.unwrap().disposition, "not_started");
        assert!(!root.join(".lake").exists());
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN"]
    async fn dropped_configuration_awaiter_cancels_its_actual_owned_scope() {
        let temp = tempfile::Builder::new()
            .prefix("lake-options-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap().join("Project");
        stage_root(
            &root,
            r#"import Lake
open Lean Lake DSL
run_cmd do
  IO.FS.writeFile (__dir__ / "configuration-started") "started"
  while !(← (__dir__ / "release").pathExists) do IO.sleep 10
  IO.FS.writeFile (__dir__ / "late-effect") "must not run after cancellation"
package dropped_options
lean_lib Demo
"#,
        );
        std::fs::write(root.join("Demo.lean"), "def sample := 1\n").unwrap();
        let broker = crate::broker::ProjectBroker::new(crate::broker::BrokerConfig {
            lake_root: None,
            max_workers: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            max_lean_runs: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        });
        let activity = broker.preparation_activity();
        let task_root = root.clone();
        let runs = broker.lean_runs();
        let task = tokio::spawn(async move {
            resolve(
                &task_root,
                LeanWorkerElabOptions::new().file_label("Demo.lean"),
                runs,
                Some(Box::new(activity)),
            )
            .await
        });
        wait_for_file(&root.join("configuration-started")).await;
        let busy = broker.runtime_status();
        assert_eq!(busy.pool.outstanding_jobs, 1);
        assert!(!busy.pool.workers_stopped);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The run returns its permit only once its process group has exited.
        tokio::time::timeout(Duration::from_secs(8), async {
            while broker.lean_runs().available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!root.join("late-effect").exists());
        assert!(!root.join(".lake").exists());
        let closed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let closed = broker.runtime_status();
                if closed.pool.workers_stopped {
                    break closed;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(closed.pool.outstanding_jobs, 0);
        assert!(closed.pool.workers_stopped);
    }

    #[tokio::test]
    async fn pre_cancelled_reader_admission_records_no_start_without_preparing_inputs() {
        let root = tempfile::tempdir().unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        request.cancel();
        let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            reader_output(
                LAKE_OPTIONS,
                root.path(),
                Duration::from_secs(1),
                Arc::new(Semaphore::new(1)),
                None,
                |_| panic!("pre-cancelled reader must not prepare inputs"),
            )
            .await
        })
        .await;
        assert_eq!(result.unwrap_err().disposition, Disposition::NotStarted);
        assert_eq!(evidence.not_started, 1);
        assert_eq!(evidence.pending, 0);
        assert!(!evidence.uncertain);
    }

    #[tokio::test]
    async fn a_run_that_gets_no_permit_by_its_deadline_never_starts() {
        let root = tempfile::tempdir().unwrap();
        let runs = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&runs).acquire_owned().await.unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            reader_output(
                "Lean file driver",
                root.path(),
                Duration::from_millis(50),
                runs,
                None,
                |_| panic!("a run without a permit must not prepare inputs"),
            )
            .await
        })
        .await;
        let failure = result.unwrap_err();
        assert_eq!(failure.disposition, Disposition::NotStarted);
        assert!(failure.error.to_string().contains("broker.max_lean_runs"));
        assert_eq!(evidence.not_started, 1);
        assert!(!evidence.uncertain);
    }
}
