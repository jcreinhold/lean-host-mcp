//! Opt-in regression on real rebuilt artifacts: a check of an unchanged file
//! sees a dependency that another process rebuilt under the shared writer.
//! Set `LEAN_HOST_MCP_TEST_FIXTURE` or `LEAN_SYSROOT` and install its matching worker.

mod support;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use lean_host_mcp::tools::semantic::{SemanticResponse, SemanticToolRequest, lean_status};
use lean_host_mcp::tools::{ToolConfig, ToolContext, as_call};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectRuntimeConfig};
use lean_toolchain::shared_writer::SharedWriter;
use serde_json::{Value, json};
use support::TestResult;
use support::chain::{Chain, chain_project};

const WRITER_LIMIT: Duration = Duration::from_mins(1);

/// Rebuild the dependency the way another process does: Lake runs in `dep/`
/// under the shared writer's turn there. The writer blocks, so it runs off
/// the test's runtime, where the host's reaper must keep running.
async fn rebuild_dep(chain: &Chain, value: u32) -> TestResult {
    chain.write("dep/Dep.lean", &format!("def dep : Nat := {value}\n"))?;
    let dependency = chain.root.join("dep");
    let mut command = Command::new(chain.sysroot.join("bin/lake"));
    command
        .arg("build")
        .current_dir(&dependency)
        .env("LEAN_SYSROOT", &chain.sysroot)
        .env("LAKE_ARTIFACT_CACHE", "false")
        .env_remove("LEAN_PATH");
    let result = tokio::task::spawn_blocking(move || {
        let helper = Path::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        let mut writer = SharedWriter::open(helper, &dependency, WRITER_LIMIT)?;
        let result = writer.run(&command, WRITER_LIMIT)?;
        writer.close(WRITER_LIMIT)?;
        std::io::Result::Ok(result)
    })
    .await??;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!("the rebuild failed: {result:?}").into())
    }
}

async fn diagnostics(ctx: &ToolContext) -> TestResult<SemanticResponse<Value>> {
    let request = SemanticToolRequest {
        kind: Some("file_diagnostics".to_owned()),
        args: std::iter::once(("file".to_owned(), json!("Chain/Query.lean"))).collect(),
    };
    Ok(as_call(lean_status(ctx, request)).await?)
}

/// Fail unless Lean checked the file without an error and `#eval` printed
/// `expected`.
fn assert_evaluated(response: &SemanticResponse<Value>, expected: &str) -> TestResult {
    let data = response.data.as_ref();
    let errors = data
        .and_then(|data| data.pointer("/diagnostics/summary/errors"))
        .and_then(Value::as_u64);
    let printed = data
        .and_then(|data| data.pointer("/diagnostics/diagnostics"))
        .and_then(Value::as_array)
        .is_some_and(|diagnostics| {
            diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.trim() == expected)
            })
        });
    let tool_errors = response
        .errors
        .iter()
        .any(|issue| issue.severity.as_deref() == Some("error"));
    if errors == Some(0) && printed && !tool_errors {
        Ok(())
    } else {
        let (issues, data) = (&response.errors, &response.data);
        Err(format!("expected #eval output {expected}: {issues:?} {data:?}").into())
    }
}

#[tokio::test]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT and a matching installed worker"]
async fn unchanged_query_sees_a_dependency_rebuilt_by_another_writer() -> TestResult {
    let chain = chain_project()?;
    chain.write("Chain/Query.lean", "import Dep\n#eval dep\n")?;
    lean_rs_writer::use_helper_executable(Path::new(env!("CARGO_BIN_EXE_lean-host-mcp")))?;
    let ctx = ToolContext {
        broker: ProjectBroker::new_with_runtime_config(
            BrokerConfig::serving(chain.root.clone()),
            ProjectRuntimeConfig::default(),
        ),
        config: ToolConfig::default(),
    };
    assert_evaluated(&diagnostics(&ctx).await?, "1")?;
    rebuild_dep(&chain, 2).await?;
    assert_evaluated(&diagnostics(&ctx).await?, "2")?;
    ctx.broker.shutdown_all();
    Ok(())
}
