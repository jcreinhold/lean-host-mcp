//! Opt-in regression using real rebuilt artifacts, entirely inside a tempdir.
//! Set `LEAN_HOST_MCP_TEST_FIXTURE` or `LEAN_SYSROOT` and install its matching worker.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use lean_host_mcp::tools::semantic::{SemanticResponse, SemanticToolRequest, lean_status};
use lean_host_mcp::tools::{TelemetryVerbosity, ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker, RuntimeFacts, ToolchainId};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn compiler_and_pin() -> TestResult<(PathBuf, String)> {
    let fixture_pin = std::env::var_os("LEAN_HOST_MCP_TEST_FIXTURE")
        .map(|root| fs::read_to_string(PathBuf::from(root).join("lean-toolchain")))
        .transpose()?;
    let sysroot = match std::env::var_os("LEAN_SYSROOT") {
        Some(root) => PathBuf::from(root),
        None => ToolchainId::parse(
            fixture_pin
                .as_deref()
                .ok_or("set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT for this ignored test")?,
        )?
        .elan_dir()?,
    };
    let compiler = sysroot.join("bin/lean");
    let pin = if let Some(pin) = fixture_pin {
        pin.trim().to_owned()
    } else {
        let version = Command::new(&compiler).arg("--version").output()?;
        assert!(version.status.success(), "lean --version failed: {version:?}");
        let output = String::from_utf8(version.stdout)?;
        let version = output
            .split_once("version ")
            .and_then(|(_, rest)| rest.split_once(','))
            .map(|(version, _)| version)
            .ok_or("could not identify compiler version")?;
        format!("leanprover/lean4:v{version}")
    };
    Ok((compiler, pin))
}

fn rebuild_import(compiler: &Path, dependency: &Path, value: u32) -> TestResult {
    fs::write(
        dependency.join("Imported.lean"),
        format!("def importedValue : Nat := {value}\n"),
    )?;
    let output = dependency.join(".lake/build/lib/lean/Imported.olean");
    fs::create_dir_all(output.parent().ok_or("artifact has no parent")?)?;
    let result = Command::new(compiler)
        .current_dir(dependency)
        .arg("-o")
        .arg(&output)
        .arg("Imported.lean")
        .output()?;
    assert!(result.status.success(), "real import rebuild failed: {result:?}");
    Ok(())
}

async fn diagnostics(ctx: &ToolContext) -> TestResult<SemanticResponse<Value>> {
    Ok(lean_status(
        ctx,
        SemanticToolRequest {
            kind: Some("file_diagnostics".to_owned()),
            args: std::iter::once(("file".to_owned(), json!("Query.lean"))).collect(),
        },
    )
    .await?)
}

fn runtime(response: &SemanticResponse<Value>) -> TestResult<&RuntimeFacts> {
    response
        .telemetry
        .as_ref()
        .and_then(|telemetry| telemetry.runtime.as_ref())
        .ok_or_else(|| "full telemetry must include worker runtime facts".into())
}

fn assert_evaluated(response: &SemanticResponse<Value>, expected: &str) {
    assert!(
        response
            .errors
            .iter()
            .all(|issue| issue.severity.as_deref() != Some("error")),
        "tool returned errors: {:?}",
        response.errors
    );
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(|data| data.pointer("/diagnostics/summary/errors"))
            .and_then(Value::as_u64),
        Some(0),
        "Lean must elaborate successfully: {:?}",
        response.data
    );
    assert!(
        response
            .data
            .as_ref()
            .and_then(|data| data.pointer("/diagnostics/diagnostics"))
            .and_then(Value::as_array)
            .is_some_and(|diagnostics| diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.trim() == expected)
            })),
        "expected #eval output {expected}: {:?}",
        response.data
    );
}

#[tokio::test]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT and a matching installed worker"]
async fn unchanged_query_observes_rebuilt_path_dependency_and_recycles_worker() -> TestResult {
    let (compiler, pin) = compiler_and_pin()?;
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("project");
    let dependency = temp.path().join("dep");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&dependency)?;
    fs::write(root.join("lakefile.toml"), "name = \"artifact_query\"\n")?;
    fs::write(dependency.join("lakefile.toml"), "name = \"artifact_dep\"\n")?;
    fs::write(root.join("lean-toolchain"), &pin)?;
    fs::write(dependency.join("lean-toolchain"), &pin)?;
    fs::write(
        root.join("lake-manifest.json"),
        serde_json::to_vec(&json!({
            "version": "1.2.0", "packagesDir": "unused-vendor", "packages": [
                {"name": "artifact_dep", "type": "path", "dir": "../dep"}
            ]
        }))?,
    )?;
    let source = "import Imported\n#eval importedValue\n";
    fs::write(root.join("Query.lean"), source)?;
    rebuild_import(&compiler, &dependency, 1)?;
    let ctx = ToolContext {
        broker: ProjectBroker::new(BrokerConfig {
            config_default: None,
            env_default: Some(root.clone()),
            cwd: root.clone(),
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: Duration::ZERO,
        }),
        config: ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    };
    let first = diagnostics(&ctx).await?;
    assert_evaluated(&first, "1");
    let first_generation = runtime(&first)?.worker_generation;
    let warm = diagnostics(&ctx).await?;
    assert_evaluated(&warm, "1");
    assert_eq!(runtime(&warm)?.worker_generation, first_generation);
    // Host cache hits use the zero-cost runtime snapshot rather than the
    // worker call's measured facts. No sleep or timing threshold is required.
    assert_eq!(runtime(&warm)?.call_elapsed_millis, 0);
    assert!(runtime(&warm)?.call_restart.is_none());

    rebuild_import(&compiler, &dependency, 2)?;
    assert_eq!(fs::read_to_string(root.join("Query.lean"))?, source);
    let rebuilt = diagnostics(&ctx).await?;
    assert_evaluated(&rebuilt, "2");
    let refreshed = runtime(&rebuilt)?;
    assert!(refreshed.worker_generation > first_generation);
    assert_eq!(
        refreshed.call_restart.as_ref().map(|event| event.cause.as_str()),
        Some("artifacts_rebuilt")
    );
    ctx.broker.shutdown_all();
    Ok(())
}
