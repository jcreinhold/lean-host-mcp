//! Resolve compilation options through the pinned Lake API, without building imports.
//!
//! Resolve before looking up a query cache. Configuration can run arbitrary Lean code,
//! including reading other files or environment variables, so we do not pretend that
//! hashing only `lakefile.lean` is sufficient to cache its evaluated options.

use std::collections::BTreeMap;
#[cfg(test)]
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lean_rs_worker_parent::LeanWorkerElabOptions;
use lean_toolchain::LeanOptionValue;
use lean_toolchain::shared_writer::{WriterDisposition, WriterFailure};
use serde::Deserialize;

use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{Result, ServerError};
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
    disposition: WriterDisposition,
}

type PreparationResult<T> = std::result::Result<T, PreparationFailure>;

impl PreparationFailure {
    fn stopped(error: ServerError) -> Self {
        Self {
            error,
            disposition: WriterDisposition::Stopped,
        }
    }

    fn unknown(error: ServerError) -> Self {
        Self {
            error,
            disposition: WriterDisposition::Unknown,
        }
    }

    fn writer(label: &str, error: std::io::Error) -> Self {
        let disposition =
            WriterFailure::from_io(&error).map_or(WriterDisposition::Unknown, |failure| failure.disposition);
        Self {
            error: reader_error(label, error),
            disposition,
        }
    }
}

impl From<ServerError> for PreparationFailure {
    fn from(error: ServerError) -> Self {
        // These conversions precede command delivery: metadata reads and private
        // helper inputs cannot have evaluated Lake configuration. Post-delivery
        // failures use the writer's typed evidence or an explicit closed outcome.
        Self {
            error,
            disposition: WriterDisposition::NotStarted,
        }
    }
}

/// Evaluate the owning package, then carry its options through every worker/cache path.
pub(crate) async fn resolve(
    root: &Path,
    options: LeanWorkerElabOptions,
    capacity_limit: usize,
    keepalive: Option<Box<dyn Send>>,
) -> Result<LeanWorkerElabOptions> {
    resolve_using(root, options, capacity_limit, None, keepalive).await
}

async fn resolve_using(
    root: &Path,
    options: LeanWorkerElabOptions,
    capacity_limit: usize,
    contract: Option<crate::shared_writer::WriterContract>,
    keepalive: Option<Box<dyn Send>>,
) -> Result<LeanWorkerElabOptions> {
    let file_label = options.file_label.clone();
    resolve_inner(root, options, capacity_limit, contract, keepalive)
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
                WriterDisposition::NotStarted => operation.not_started(),
                // Confirmed closure settles execution even though this failed
                // preparation supplies no option data or native-worker receipt.
                WriterDisposition::Stopped => operation.returned(),
                WriterDisposition::Unknown => operation.unknown(),
            }
            Err(preparation_failed(file_label, error))
        }
    }
}

/// Exercise preparation's real child/effect boundary without a Lean toolchain.
#[cfg(test)]
pub(crate) async fn successful_preparation_for_test(effect_path: &Path) -> Result<LeanWorkerElabOptions> {
    observe_preparation("PreparationFixture.lean".to_owned(), async {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf prepared > \"$1\"")
            .arg("lean-host-mcp-preparation-test")
            .arg(effect_path)
            .kill_on_drop(true);
        let output = test_child_output(&mut command, Duration::from_secs(5))
            .await
            .map_err(PreparationFailure::unknown)?;
        if !output.status.success() {
            return Err(PreparationFailure::stopped(bad_project(format!(
                "test preparation child exited: {}",
                output.status
            ))));
        }
        Ok(LeanWorkerElabOptions::new().file_label("PreparationFixture.lean"))
    })
    .await
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
    capacity_limit: usize,
    contract: Option<crate::shared_writer::WriterContract>,
    keepalive: Option<Box<dyn Send>>,
) -> PreparationResult<LeanWorkerElabOptions> {
    // This is the SAME full project union the utility guardian will acquire.
    // Retain it in the blocking owner until guardian custody/closure, so package
    // and toolchain metadata cannot change between resolution and execution.
    // There is no partial lock set or lock-order upgrade across the handoff.
    let preflight = CallCancellation::current().begin_operation();
    let context: PreparationResult<_> = (|| {
        let custody = crate::artifact_reads::ArtifactReadGuard::try_acquire(root).map_err(bad_project)?;
        let toolchain = ToolchainId::from_lake_root(root).map_err(bad_project)?;
        let sysroot = toolchain.elan_dir().map_err(bad_project)?;
        let file = source_path(root, &options.file_label);
        let (package, config_file, dependency_name) = owning_config(root, file.as_deref())?;
        let canonical_root = root.canonicalize().map_err(bad_project)?;
        Ok((
            custody,
            sysroot,
            file,
            package,
            config_file,
            dependency_name,
            canonical_root,
        ))
    })();
    // Pure metadata and lock admission have not released a configuration command.
    preflight.not_started();
    let (custody, sysroot, file, package, config_file, dependency_name, root) = context?;
    let command_root = root.clone();
    let command_package = package.clone();
    let output = reader_output(
        LAKE_OPTIONS,
        root,
        CallCancellation::current().remaining_or(TIMEOUT),
        capacity_limit,
        custody,
        contract,
        keepalive,
        move |assets| {
            // The native-reader owner retains these private inputs until actual
            // scope closure, including a lost or cancelled async waiter.
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
            Ok(crate::native_reader::NativeReaderProgram {
                command,
                environment: vec![("LEAN_SYSROOT".into(), sysroot.into_os_string())],
                cwd: command_package,
                artifact_roots: Vec::new(),
                max_stdout_bytes: crate::native_reader::OUTPUT_LIMIT,
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

/// Cancellation signals the owner rather than dropping its process evidence.
/// The same deadline covers preparation, native admission, and command delivery.
pub(crate) async fn reader_output(
    label: &'static str,
    root: PathBuf,
    timeout: Duration,
    capacity_limit: usize,
    custody: crate::artifact_reads::ArtifactReadGuard,
    contract: Option<crate::shared_writer::WriterContract>,
    keepalive: Option<Box<dyn Send>>,
    prepare: impl FnOnce(&Path) -> std::io::Result<crate::native_reader::NativeReaderProgram> + Send + 'static,
) -> PreparationResult<Vec<u8>> {
    let cancellation = CallCancellation::current();
    let operation = cancellation.begin_operation();
    let mut on_drop = cancellation.cancel_on_drop();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        // The actual native/reaper owner retains both the broker admission and
        // this complete read union through Unknown, until scope closure.
        let keepalive: Option<Box<dyn Send>> = Some(Box::new((custody, keepalive)));
        let cancelled = || cancellation.is_cancelled();
        let remaining = timeout.saturating_sub(started.elapsed());
        let result = if remaining.is_zero() {
            Err(reader_error(label, "deadline expired before native reader admission").into())
        } else if cancelled() {
            Err(reader_error(label, "cancelled before native reader admission").into())
        } else {
            let reader = match contract {
                Some(contract) => crate::native_reader::run_program_with_contract(
                    &contract,
                    &root,
                    remaining,
                    capacity_limit,
                    &cancelled,
                    keepalive,
                    prepare,
                ),
                None => {
                    crate::native_reader::run_program(&root, remaining, capacity_limit, &cancelled, keepalive, prepare)
                }
            };
            reader
                .map_err(|error| PreparationFailure::writer(label, error))
                .and_then(|result| reader_bytes(label, result))
        };
        match result
            .as_ref()
            .map_or_else(|failure| failure.disposition, |_| WriterDisposition::Stopped)
        {
            WriterDisposition::NotStarted => operation.not_started(),
            WriterDisposition::Stopped => operation.returned(),
            WriterDisposition::Unknown => operation.unknown(),
        }
        result
    })
    .await;
    on_drop.disarm();
    result.map_err(|error| PreparationFailure::unknown(reader_error(label, error)))?
}

fn reader_bytes(label: &str, reader: crate::native_reader::NativeReaderResult) -> PreparationResult<Vec<u8>> {
    if reader.protocol == crate::native_reader::PROTOCOL
        && reader.state == "completed"
        && reader.scope_closed
        && reader.target_started == Some(true)
        && reader.exit_code == Some(0)
        && reader.exit_status.is_some()
        && !reader.output_truncated
        && !reader.intervened
    {
        return Ok(reader.stdout.into_bytes());
    }
    let disposition = if !reader.scope_closed {
        WriterDisposition::Unknown
    } else if reader.state == "not_started" && reader.target_started == Some(false) {
        WriterDisposition::NotStarted
    } else {
        WriterDisposition::Stopped
    };
    let target_note = if label == LAKE_OPTIONS {
        "; no proof check ran"
    } else {
        ""
    };
    Err(PreparationFailure {
        disposition,
        error: reader_error(
            label,
            format!(
                "native reader did not return a complete result{target_note}: state={} {}{}{}",
                reader.state,
                reader.detail.unwrap_or_default(),
                reader.stdout,
                reader.stderr,
            ),
        ),
    })
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

    fn reader_result() -> crate::native_reader::NativeReaderResult {
        crate::native_reader::NativeReaderResult {
            protocol: 1,
            state: "completed".into(),
            target_started: Some(true),
            exit_code: Some(0),
            exit_status: Some("exit status: 0".into()),
            stdout: "{\"module\":\"Demo\",\"options\":{}}".into(),
            stderr: String::new(),
            output_truncated: false,
            diagnostic_preview: None,
            intervened: false,
            scope_closed: true,
            protected_domains: Vec::new(),
            runtime_compatibility: serde_json::Value::Null,
            stdin_sha256: None,
            stdin_bytes: None,
            payload_usage: None,
            detail: None,
        }
    }

    #[test]
    fn discovery_requires_complete_uninterrupted_closed_reader_result() {
        assert!(reader_bytes(LAKE_OPTIONS, reader_result()).is_ok());
        for mutation in 0..5 {
            let mut reader = reader_result();
            match mutation {
                0 => reader.intervened = true,
                1 => reader.output_truncated = true,
                2 => reader.exit_status = None,
                3 => reader.exit_code = Some(1),
                _ => reader.state = "stopped".into(),
            }
            let error = reader_bytes(LAKE_OPTIONS, reader).unwrap_err();
            assert_eq!(error.disposition, WriterDisposition::Stopped);
            assert!(error.error.to_string().contains("no proof check ran"));
        }
    }

    #[test]
    fn stopped_file_driver_reports_its_own_error_without_a_target_verdict() {
        let mut reader = reader_result();
        reader.state = "stopped".into();
        reader.intervened = true;
        reader.stdout.clear();
        reader.detail = Some("native reader deadline elapsed".into());
        let failure = reader_bytes("Lean file driver", reader).unwrap_err();
        assert_eq!(failure.disposition, WriterDisposition::Stopped);
        let error = rmcp::ErrorData::from(failure.error);
        assert_eq!(
            error,
            rmcp::ErrorData::internal_error(
                "lake project not usable: Lean file driver: native reader did not return a complete result: state=stopped native reader deadline elapsed",
                None,
            )
        );
    }

    #[test]
    fn discovery_distinguishes_unstarted_from_unclosed_preparation() {
        let mut reader = reader_result();
        reader.state = "not_started".into();
        reader.target_started = Some(false);
        let error = reader_bytes(LAKE_OPTIONS, reader).unwrap_err();
        assert_eq!(error.disposition, WriterDisposition::NotStarted);
        let mut reader = reader_result();
        reader.state = "unknown".into();
        reader.target_started = Some(false);
        reader.scope_closed = false;
        let error = reader_bytes(LAKE_OPTIONS, reader).unwrap_err();
        assert_eq!(error.disposition, WriterDisposition::Unknown);
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

    fn fixture_contract(root: &Path) -> crate::shared_writer::WriterContract {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = PathBuf::from(std::env::var_os("LEAN_HOST_MCP_OPTIONS_TEST_HELPER").unwrap())
            .canonicalize()
            .unwrap();
        let registry = root.parent().unwrap().join("Owners");
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::shared_writer::WriterContract {
            protocol: 1,
            executable,
            ownership_registry: registry,
        }
    }

    async fn fixture_resolve(root: &Path, label: &str) -> Result<LeanWorkerElabOptions> {
        resolve_using(
            root,
            LeanWorkerElabOptions::new().file_label(label),
            1,
            Some(fixture_contract(root)),
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
    #[ignore = "requires pinned Lean plus actual H via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN/HELPER"]
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
        // A resident generation retains these same SH domains. Old EX-based
        // discovery would wait its full 30-second deadline on the second call.
        let warm = crate::artifact_reads::ArtifactReadGuard::try_acquire(&root).unwrap();
        let loose = tokio::time::timeout(Duration::from_secs(8), fixture_resolve(&root, "Loose.lean"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loose.lean_options["autoImplicit"], LeanOptionValue::Bool(true));
        drop(warm);
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
    #[ignore = "requires pinned Lean plus actual H via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN/HELPER"]
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

    struct ResidentFixture {
        child: Option<std::process::Child>,
        directory: Option<tempfile::TempDir>,
        closed: bool,
    }
    impl Drop for ResidentFixture {
        fn drop(&mut self) {
            if let Some(child) = &mut self.child {
                if matches!(child.try_wait(), Ok(None)) {
                    drop(child.kill());
                }
                drop(child.wait());
            }
            if !self.closed
                && let Some(directory) = self.directory.take()
            {
                let retained = directory.keep();
                eprintln!(
                    "retained native fixture inputs after unconfirmed completion: {}",
                    retained.display()
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean plus actual H via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN/HELPER"]
    async fn resident_native_scope_at_limit_one_allows_warm_options_without_recycle() {
        let temp = tempfile::Builder::new()
            .prefix("lake-options-")
            .tempdir_in("/tmp")
            .unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("Project");
        stage_root(
            &root,
            "import Lake\nopen Lake DSL\npackage warm_options\nlean_lib Demo\n",
        );
        std::fs::write(root.join("Demo.lean"), "def sample := 1\n").unwrap();
        let contract = fixture_contract(&root);
        let started = base.join("resident-started");
        let release = base.join("release-resident");
        let child = std::process::Command::new(&contract.executable)
            .args(["native-reader", "--project-root"])
            .arg(&root)
            .args(["--capacity-limit", "1", "--timeout-millis", "30000", "--"])
            .arg(Path::new("/bin/sh").canonicalize().unwrap())
            .args([
                "-c",
                "printf '%s' \"$$\" > \"$1\"; while [ ! -f \"$2\" ]; do /bin/sleep 0.01; done",
                "resident-native-fixture",
            ])
            .arg(&started)
            .arg(&release)
            .env(
                lean_toolchain::native_guardian::OWNERS_DIRECTORY_ENV,
                &contract.ownership_registry,
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut resident = ResidentFixture {
            child: Some(child),
            directory: Some(temp),
            closed: false,
        };
        wait_for_file(&started).await;
        let original_pid = resident.child.as_ref().unwrap().id();
        let original_payload = std::fs::read_to_string(&started).unwrap();
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(8), fixture_resolve(&root, "Demo.lean"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(resident.child.as_ref().unwrap().id(), original_pid);
            assert!(resident.child.as_mut().unwrap().try_wait().unwrap().is_none());
            assert_eq!(std::fs::read_to_string(&started).unwrap(), original_payload);
        }
        std::fs::write(&release, "done").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while resident.child.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let output = resident.child.take().unwrap().wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["state"], "completed");
        resident.closed = true;
        assert!(!root.join(".lake").exists());
    }

    #[tokio::test]
    #[ignore = "requires pinned Lean plus actual H via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN/HELPER"]
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
    #[ignore = "requires pinned Lean plus actual H via LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN/HELPER"]
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
            env_default: None,
            cwd: root.clone(),
            max_projects: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        });
        let activity = broker.preparation_activity(&root).await.unwrap();
        let task_root = root.clone();
        let contract = fixture_contract(&root);
        let task = tokio::spawn(async move {
            resolve_using(
                &task_root,
                LeanWorkerElabOptions::new().file_label("Demo.lean"),
                1,
                Some(contract),
                Some(Box::new(activity)),
            )
            .await
        });
        wait_for_file(&root.join("configuration-started")).await;
        let busy = broker.runtime_control(&crate::broker::RuntimeControlRequest {
            action: crate::broker::RuntimeAction::Acquire,
            ..crate::broker::RuntimeControlRequest::default()
        });
        assert_eq!(busy.status, crate::broker::RuntimeControlStatus::Busy);
        assert_eq!(busy.pool.outstanding_jobs, 1);
        assert!(!busy.pool.workers_stopped);
        let draining = broker.runtime_control(&crate::broker::RuntimeControlRequest {
            action: crate::broker::RuntimeAction::Drain,
            ..crate::broker::RuntimeControlRequest::default()
        });
        assert_eq!(draining.status, crate::broker::RuntimeControlStatus::Draining);
        let lease = draining.lease_id.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // EX is only obtainable after every native guardian loan has closed.
        let path = lean_toolchain::shared_writer::project_lock_path(&root).unwrap();
        let lock = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match lock.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(5)).await,
                    Err(error) => panic!("native scope cleanup lock failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(!root.join("late-effect").exists());
        assert!(!root.join(".lake").exists());
        let closed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let closed = broker.runtime_control(&crate::broker::RuntimeControlRequest {
                    action: crate::broker::RuntimeAction::Renew,
                    lease_id: Some(lease.clone()),
                    ..crate::broker::RuntimeControlRequest::default()
                });
                if closed.pool.workers_stopped {
                    break closed;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(closed.status, crate::broker::RuntimeControlStatus::Ok);
        assert_eq!(closed.pool.outstanding_jobs, 0);
        assert!(closed.pool.workers_stopped);
    }

    #[tokio::test]
    async fn pre_cancelled_reader_admission_records_no_start_without_preparing_inputs() {
        let root = tempfile::tempdir().unwrap();
        let custody = crate::artifact_reads::ArtifactReadGuard::try_acquire(root.path()).unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        request.cancel();
        let (result, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            reader_output(
                LAKE_OPTIONS,
                root.path().to_path_buf(),
                Duration::from_secs(1),
                1,
                custody,
                None,
                None,
                |_| panic!("pre-cancelled reader must not prepare inputs"),
            )
            .await
        })
        .await;
        assert_eq!(result.unwrap_err().disposition, WriterDisposition::NotStarted);
        assert_eq!(evidence.not_started, 1);
        assert_eq!(evidence.pending, 0);
        assert!(!evidence.uncertain);
    }
}
