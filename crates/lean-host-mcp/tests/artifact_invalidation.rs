//! Opt-in regression on real rebuilt artifacts: a check of an unchanged file
//! sees a dependency that `lean_build` rebuilt.
//! Set `LEAN_HOST_MCP_TEST_FIXTURE` or `LEAN_SYSROOT` and install its matching worker.

use lean_host_mcp::build::BuildRequest;
use lean_host_mcp::tools::build::lean_build;
use lean_host_mcp::tools::semantic::{SemanticResponse, SemanticToolRequest, lean_status};
use lean_host_mcp::tools::{ToolConfig, ToolContext, as_call};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectRuntimeConfig};
use lean_host_mcp_test_support as support;
use serde_json::{Value, json};
use support::TestResult;
use support::chain::{Chain, chain_project};

/// Change the dependency and rebuild it through the server, which stops the
/// workers that loaded the old build.
async fn rebuild_dep(ctx: &ToolContext, chain: &Chain, value: u32) -> TestResult {
    chain.write("dep/Dep.lean", &format!("def dep : Nat := {value}\n"))?;
    let request = BuildRequest {
        targets: vec!["Dep".to_owned()],
        ..BuildRequest::default()
    };
    let outcome = as_call(lean_build(ctx, request)).await?;
    if outcome.ok {
        Ok(())
    } else {
        Err(format!("the rebuild failed: {outcome:?}").into())
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
async fn unchanged_query_sees_a_rebuilt_dependency() -> TestResult {
    let chain = chain_project(env!("CARGO_TARGET_TMPDIR"))?;
    chain.write("Chain/Query.lean", "import Dep\n#eval dep\n")?;
    let ctx = ToolContext {
        broker: ProjectBroker::new_with_runtime_config(
            BrokerConfig::serving(chain.root.clone()),
            ProjectRuntimeConfig::default(),
        ),
        config: ToolConfig::default(),
    };
    assert_evaluated(&diagnostics(&ctx).await?, "1")?;
    rebuild_dep(&ctx, &chain, 2).await?;
    assert_evaluated(&diagnostics(&ctx).await?, "2")?;
    ctx.broker.shutdown_all();
    Ok(())
}
