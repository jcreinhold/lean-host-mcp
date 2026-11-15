//! Resolve compilation options through the pinned Lake API, without building imports.
//!
//! Resolve before looking up a query cache. Configuration can run arbitrary Lean code,
//! including reading other files or environment variables, so we do not pretend that
//! hashing only `lakefile.lean` is sufficient to cache its evaluated options.

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lean_rs_worker_parent::LeanWorkerElabOptions;
use lean_toolchain::LeanOptionValue;
use serde::Deserialize;

use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{Result, ServerError};
use crate::toolchain::ToolchainId;

const HELPER: &str = include_str!("lake/ModuleOptions.lean");
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct ModuleOptions {
    module: String,
    options: BTreeMap<String, LeanOptionValue>,
}

/// Evaluate the owning package, then carry its options through every worker/cache path.
pub(crate) async fn resolve(root: &Path, options: LeanWorkerElabOptions) -> Result<LeanWorkerElabOptions> {
    observe_preparation(options.file_label.clone(), resolve_inner(root, options)).await
}

async fn observe_preparation(
    file_label: String,
    preparation: impl Future<Output = Result<LeanWorkerElabOptions>>,
) -> Result<LeanWorkerElabOptions> {
    let operation = CallCancellation::current().begin_operation();
    match preparation.await {
        Ok(options) => {
            operation.returned();
            Ok(options)
        }
        Err(error) => {
            operation.unknown();
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
        let output = discovery_output(&mut command, Duration::from_secs(5)).await?;
        if !output.status.success() {
            return Err(bad_project(format!("test preparation child exited: {}", output.status)));
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
    CallCancellation::current().preparation_failed(target_check.clone());
    let message = if let ServerError::BadProject(message) = error {
        message
    } else {
        error.to_string()
    };
    ServerError::PreparationFailed { message, target_check }
}

async fn resolve_inner(root: &Path, options: LeanWorkerElabOptions) -> Result<LeanWorkerElabOptions> {
    let toolchain = ToolchainId::from_lake_root(root).map_err(bad_project)?;
    let sysroot = toolchain.elan_dir().map_err(bad_project)?;
    let file = source_path(root, &options.file_label);
    let (package, config_file, dependency_name) = owning_config(root, file.as_deref())?;
    let config_workspace = tempfile::tempdir().map_err(bad_project)?;
    let mut helper = tempfile::Builder::new()
        .suffix(".lean")
        .tempfile()
        .map_err(bad_project)?;
    helper.write_all(HELPER.as_bytes()).map_err(bad_project)?;
    let plugin = sysroot.join("lib/lean").join(if cfg!(target_os = "macos") {
        "libLake_shared.dylib"
    } else {
        "libLake_shared.so"
    });
    let mut command = tokio::process::Command::new(sysroot.join("bin/lean"));
    command
        .current_dir(&package)
        .env("LEAN_SYSROOT", &sysroot)
        .arg("--plugin")
        .arg(plugin)
        .arg("--run")
        .arg(helper.path())
        .arg(root)
        .arg(&package)
        .arg(config_file)
        .arg(dependency_name)
        .arg(config_workspace.path())
        .arg(file.as_deref().unwrap_or_else(|| Path::new("")))
        .kill_on_drop(true);
    let output = discovery_output(&mut command, TIMEOUT).await?;
    if !output.status.success() {
        return Err(ServerError::BadProject(format!(
            "could not resolve Lake compilation options (no proof check ran): {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    let resolved: ModuleOptions = serde_json::from_slice(&output.stdout).map_err(|error| {
        ServerError::BadProject(format!("invalid Lake option metadata: {error}; no proof check ran"))
    })?;
    tracing::debug!(
        package = %package.display(), module = %resolved.module,
        lean_options = ?resolved.options, heartbeat_limit = options.heartbeat_limit,
        "resolved Lake compilation options; host heartbeat limit overrides project maxHeartbeats"
    );
    Ok(options.lean_options(resolved.options))
}

async fn discovery_output(command: &mut tokio::process::Command, timeout: Duration) -> Result<std::process::Output> {
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
    ServerError::BadProject(format!("Lake option discovery: {error}"))
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

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
            let error = discovery_output(&mut command, Duration::from_millis(30))
                .await
                .unwrap_err();
            preparation_failed("UnitIndexed.lean".to_owned(), error)
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

    #[tokio::test]
    #[ignore = "requires an installed Lean toolchain; set LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN"]
    async fn lake_module_options_follow_config_without_builds() {
        let pin = std::env::var("LEAN_HOST_MCP_OPTIONS_TEST_TOOLCHAIN").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("lean-toolchain"), pin).unwrap();
        let config = r#"import Lake
open Lake DSL
require dependency from ".lake/packages/dependency" with NameMap.empty.insert `strict "true"
package options_test where
  leanOptions := #[⟨`autoImplicit, true⟩]
  moreLeanArgs := #["-D", "maxRecDepth=321", "-DautoImplicit=false", "--define=weak.testUnused=true"]
lean_lib Strict where
  leanOptions := #[⟨`autoImplicit, false⟩]
lean_lib Loose
"#;
        std::fs::write(root.join("lakefile.lean"), config).unwrap();
        std::fs::write(root.join("Strict.lean"), "def sample := 1\n").unwrap();
        std::fs::write(root.join("Loose.lean"), "def sample := 1\n").unwrap();
        let strict = LeanWorkerElabOptions::new().file_label("Strict.lean");
        let resolved = resolve(&root, strict.clone()).await.unwrap();
        assert_eq!(resolved.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert_eq!(resolved.lean_options["maxRecDepth"], LeanOptionValue::Nat(321));
        assert_eq!(
            resolved.lean_options["weak.testUnused"],
            LeanOptionValue::String("true".into())
        );
        let loose = resolve(&root, LeanWorkerElabOptions::new().file_label("Loose.lean"))
            .await
            .unwrap();
        assert_eq!(loose.lean_options["autoImplicit"], LeanOptionValue::Bool(true));
        assert!(!root.join(".lake/build").exists());
        std::fs::write(
            root.join("lakefile.lean"),
            config.replace("⟨`autoImplicit, false⟩", "⟨`autoImplicit, true⟩"),
        )
        .unwrap();
        let changed = resolve(&root, strict).await.unwrap();
        assert_eq!(changed.lean_options["autoImplicit"], LeanOptionValue::Bool(true));
        assert!(!root.join(".lake/build").exists());
        let dependency = root.join(".lake/packages/dependency");
        std::fs::create_dir_all(&dependency).unwrap();
        std::fs::write(
            dependency.join("lakefile.toml"),
            "name = 'dependency'\n[leanOptions]\nautoImplicit = false\n[[lean_lib]]\nname = 'Dependency'\n",
        )
        .unwrap();
        std::fs::write(root.join("lake-manifest.json"), serde_json::json!({
            "packages": [{"name": "dependency", "type": "path", "dir": ".lake/packages/dependency", "configFile": "lakefile.toml"}]
        }).to_string()).unwrap();
        let source = dependency.join("Dependency.lean");
        std::fs::write(&source, "def sample := 1\n").unwrap();
        let dep = resolve(&root, LeanWorkerElabOptions::new().file_label(source.to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(dep.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert!(!dep.lean_options.contains_key("maxRecDepth"));
        assert!(!dependency.join(".lake/build").exists());
        assert!(
            !dependency.join(".lake/config").exists(),
            "config compilation must use temporary storage"
        );
        std::fs::rename(dependency.join("lakefile.toml"), dependency.join("custom.toml")).unwrap();
        std::fs::write(root.join("lake-manifest.json"), serde_json::json!({
            "packages": [{"name": "dependency", "type": "path", "dir": ".lake/packages/dependency", "configFile": "custom.toml"}]
        }).to_string()).unwrap();
        let dep = resolve(&root, LeanWorkerElabOptions::new().file_label(source.to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(dep.lean_options["autoImplicit"], LeanOptionValue::Bool(false));
        assert!(!dep.lean_options.contains_key("maxRecDepth"));
        let shared_config = dependency.join(".lake/config/0");
        std::fs::create_dir_all(&shared_config).unwrap();
        std::fs::write(shared_config.join("custom.olean"), "existing build config").unwrap();
        std::fs::write(shared_config.join("custom.olean.trace"), "existing build trace").unwrap();
        std::fs::write(dependency.join("custom.lean"), "import Lake\nopen Lake DSL\npackage dependency\nlean_lib Dependency where\n  leanOptions := #[⟨`autoImplicit, (get_config? strict).isNone⟩]\n").unwrap();
        let manifest = serde_json::json!({"packages": [{"name": "dependency", "type": "path", "dir": ".lake/packages/dependency", "configFile": "custom.lean"}]}).to_string();
        std::fs::write(root.join("lake-manifest.json"), &manifest).unwrap();
        let dep = resolve(&root, LeanWorkerElabOptions::new().file_label(source.to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(
            dep.lean_options["autoImplicit"],
            LeanOptionValue::Bool(false),
            "require-with option must reach dependency config"
        );
        std::fs::write(
            root.join("lakefile.lean"),
            config.replace(" with NameMap.empty.insert `strict \"true\"", ""),
        )
        .unwrap();
        let changed_dep = resolve(&root, LeanWorkerElabOptions::new().file_label(source.to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(
            changed_dep.lean_options["autoImplicit"],
            LeanOptionValue::Bool(true),
            "changed require-with options must not reuse old configuration"
        );

        std::fs::write(
            root.join("lakefile.lean"),
            config.replace(
                "require dependency from \".lake/packages/dependency\" with NameMap.empty.insert `strict \"true\"\n",
                "",
            ),
        )
        .unwrap();
        assert!(
            resolve(&root, LeanWorkerElabOptions::new().file_label(source.to_str().unwrap()))
                .await
                .is_err(),
            "transitive dependency must not silently use defaults"
        );
        assert_eq!(
            std::fs::read_to_string(shared_config.join("custom.olean")).unwrap(),
            "existing build config"
        );
        assert_eq!(
            std::fs::read_to_string(shared_config.join("custom.olean.trace")).unwrap(),
            "existing build trace"
        );
    }
}
