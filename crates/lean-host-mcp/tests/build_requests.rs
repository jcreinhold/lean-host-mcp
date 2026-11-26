//! Builds and import lists through `lean_build` and `lean_imports`, run in
//! process on a small Lake project with a chain of imports.
//!
//! Every test here is ignored. Set `LEAN_HOST_MCP_TEST_FIXTURE` or
//! `LEAN_SYSROOT` to run them; the tests that check files also need a worker
//! installed for the fixture's toolchain.
//!
//! The tests run one at a time. The SDK's writer helper refuses to start
//! while any process in the shared owner registry is starting, even one for
//! another project, so a build in one test would fail whenever another test
//! starts a reader or a worker at that moment.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use lean_host_mcp::build::{BuildOutcome, BuildRequest, BuildState};
use lean_host_mcp::tools::build::{ImportsError, ImportsReply, ImportsRequest, lean_build, lean_imports};
use lean_host_mcp::tools::semantic::{SemanticResponse, SemanticToolRequest, lean_status};
use lean_host_mcp::tools::{ToolConfig, ToolContext, as_call};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectRuntimeConfig};
use serde_json::{Value, json};
use serial_test::serial;
use support::TestResult;
use support::chain::{Chain, chain_project};

/// The recipes the tests run. The guard checks that the dependency is built.
const CONFIG: &str = r#"
[build]
guard = ["@dep/+Dep:olean"]

[recipes.slow]
steps = [["env", "sleep", "2"]]

[recipes.loud]
steps = [["env", "sh", "-c", "yes | head -c 200000"]]

[recipes.stuck]
steps = [["env", "sleep", "30"]]

[recipes.late]
steps = [["env", "sh", "-c", "(sleep 2; touch late-marker) & exit 0"]]

[recipes.slowb]
steps = [["env", "sleep", "2"], ["build", "Chain.B"]]
"#;

/// How long a test waits for something it expects to happen.
const PATIENCE: Duration = Duration::from_mins(2);

/// A built chain project with [`CONFIG`], and a context set up as the server
/// sets itself up for `--lake-root` there.
fn project() -> TestResult<(Chain, ToolContext)> {
    let chain = chain_project()?;
    chain.write("lean-host-mcp.toml", CONFIG)?;
    lean_host_mcp::shared_writer::use_helper_executable(Path::new(env!("CARGO_BIN_EXE_lean-host-mcp")))?;
    let broker = ProjectBroker::new_with_runtime_config(
        BrokerConfig {
            env_default: Some(chain.root.clone()),
            cwd: chain.root.clone(),
            max_projects: BrokerConfig::default_max_projects(),
            max_workers_per_project: NonZeroUsize::new(2).ok_or("two workers")?,
            idle_timeout: BrokerConfig::default_idle_timeout(),
        },
        ProjectRuntimeConfig::default().with_operator_primary(Some(&chain.root))?,
    );
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };
    Ok((chain, ctx))
}

fn recipe(name: &str) -> BuildRequest {
    BuildRequest {
        recipe: Some(name.to_owned()),
        ..BuildRequest::default()
    }
}

fn targets(names: &[&str]) -> BuildRequest {
    BuildRequest {
        targets: names.iter().map(|name| (*name).to_owned()).collect(),
        ..BuildRequest::default()
    }
}

async fn build(ctx: &ToolContext, request: BuildRequest) -> TestResult<BuildOutcome> {
    Ok(as_call(lean_build(ctx, request)).await?)
}

async fn imports(ctx: &ToolContext, files: &[&str]) -> Result<ImportsReply, ImportsError> {
    let request = ImportsRequest {
        project: None,
        files: files.iter().map(|file| (*file).to_owned()).collect(),
    };
    as_call(lean_imports(ctx, request)).await
}

async fn diagnostics(ctx: &ToolContext, file: &str) -> TestResult<SemanticResponse<Value>> {
    let request = SemanticToolRequest {
        kind: Some("file_diagnostics".to_owned()),
        args: std::iter::once(("file".to_owned(), json!(file))).collect(),
    };
    Ok(as_call(lean_status(ctx, request)).await?)
}

/// Fail unless Lean checked the file of `response` without an error.
fn assert_clean(response: &SemanticResponse<Value>) -> TestResult {
    if response.trust.session_id == "metadata-only" {
        return Err("the reply to a file check says Lean elaborated nothing".into());
    }
    let errors = response
        .data
        .as_ref()
        .and_then(|data| data.pointer("/diagnostics/summary/errors"))
        .and_then(Value::as_u64);
    let tool_errors = response
        .errors
        .iter()
        .any(|issue| issue.severity.as_deref() == Some("error"));
    if errors == Some(0) && !tool_errors {
        Ok(())
    } else {
        Err(format!("the check found errors: {:?} {:?}", response.errors, response.data).into())
    }
}

/// Wait until the log of a build in `root` holds `needle`.
async fn wait_for_log(root: &Path, needle: &str) -> TestResult {
    let logs = lean_host_mcp::store::store_dir(root, None).join("logs");
    let header = format!(" in {}]", root.display());
    let found = tokio::time::timeout(PATIENCE, async {
        loop {
            let found = fs::read_dir(&logs).into_iter().flatten().flatten().any(|entry| {
                fs::read_to_string(entry.path()).is_ok_and(|text| text.contains(&header) && text.contains(needle))
            });
            if found {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    found.map_err(|_| format!("no build log in {} shows `{needle}`", logs.display()))?;
    Ok(())
}

/// The modification time of every `.olean` of the project and its dependency.
fn olean_times(chain: &Chain) -> TestResult<BTreeMap<PathBuf, SystemTime>> {
    let mut times = BTreeMap::new();
    for dir in [chain.root.join(".lake/build"), chain.root.join("dep/.lake/build")] {
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "olean") {
                times.insert(entry.path().to_path_buf(), entry.metadata()?.modified()?);
            }
        }
    }
    Ok(times)
}

fn epoch_millis() -> TestResult<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_millis(),
    )?)
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_failing_step_ends_the_build_and_its_tail_shows_the_error() -> TestResult {
    let (chain, ctx) = project()?;
    chain.write("Chain/C.lean", "import Chain.B\n\ntheorem b_eq : b = 3 := rfl\n")?;
    let outcome = build(&ctx, targets(&["Chain"])).await?;
    assert_eq!(outcome.state, BuildState::Done);
    assert!(!outcome.ok);
    assert_eq!(outcome.steps.len(), 2, "the guard and the build ran: {outcome:?}");
    assert_eq!(outcome.failed_step, Some(1));
    assert_eq!(outcome.exit_code, Some(1));
    assert!(outcome.tail.contains("error:"), "{}", outcome.tail);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_stale_guard_stops_the_build_before_lake_writes() -> TestResult {
    let (chain, ctx) = project()?;
    chain.write("dep/Dep.lean", "def dep : Nat := 2\n")?;
    let before = olean_times(&chain)?;
    let outcome = build(&ctx, targets(&["Chain"])).await?;
    assert_eq!(outcome.steps.len(), 1, "only the guard ran: {outcome:?}");
    assert_eq!(outcome.failed_step, Some(0));
    assert!(!outcome.ok);
    assert_eq!(olean_times(&chain)?, before);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn builds_run_one_at_a_time_in_arrival_order() -> TestResult {
    let (chain, ctx) = project()?;
    let first = tokio::spawn({
        let ctx = ctx.clone();
        async move { as_call(lean_build(&ctx, recipe("slow"))).await }
    });
    wait_for_log(&chain.root, "$ lake env sleep 2").await?;
    let second = build(&ctx, targets(&["Chain.A"])).await?;
    let first = first.await??;
    assert!(first.ok && second.ok, "{first:?} {second:?}");
    let first_end = first.steps.last().ok_or("the first build ran steps")?.finished_at_ms;
    let second_start = second.steps.first().ok_or("the second build ran steps")?.started_at_ms;
    assert!(
        second_start >= first_end,
        "the second build started at {second_start}, before the first ended at {first_end}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_large_output_completes_and_the_reply_carries_its_end() -> TestResult {
    let (_chain, ctx) = project()?;
    let outcome = build(&ctx, recipe("loud")).await?;
    assert!(outcome.ok, "{:?}", outcome.steps);
    assert!(outcome.log.bytes > 200_000, "{:?}", outcome.log);
    assert!(!outcome.log.truncated);
    assert!(outcome.tail.len() <= 8192);
    assert!(outcome.tail.contains("y\ny\n"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn the_deadline_stops_a_build_and_frees_the_project() -> TestResult {
    let (chain, ctx) = project()?;
    chain.write(
        "lean-host-mcp.toml",
        &CONFIG.replacen("[build]\n", "[build]\ntimeout_secs = 2\n", 1),
    )?;
    let started = Instant::now();
    let outcome = build(&ctx, recipe("stuck")).await?;
    assert_eq!(outcome.state, BuildState::TimedOut, "{outcome:?}");
    assert!(!outcome.ok);
    assert_eq!(outcome.exit_code, None);
    assert!(started.elapsed() < Duration::from_secs(20));
    // A reader gets its turn and the shared kernel lock again.
    let reply = imports(&ctx, &["Chain/C.lean"])
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert_eq!(reply.files.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_step_cannot_leave_work_running_past_its_build() -> TestResult {
    let (chain, ctx) = project()?;
    let outcome = build(&ctx, recipe("late")).await?;
    // The writer stops work that outlives the step's own process and reports
    // no result for the step.
    let step = outcome.steps.last().ok_or("the step ran")?;
    assert_eq!(step.exit_code, None, "{outcome:?}");
    assert!(!outcome.ok);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !chain.root.join("late-marker").exists(),
        "a process of the build outlived its reply"
    );
    let next = build(&ctx, targets(&["Chain.A"])).await?;
    assert!(next.ok, "{next:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn reads_after_a_lakefile_edit_leave_the_config_cache_to_lake() -> TestResult {
    let (chain, ctx) = project()?;
    let trace = chain.root.join(".lake/config/0/lakefile.olean.trace");
    let elaborated = fs::metadata(&trace)?.modified()?;
    let lakefile = fs::read_to_string(chain.root.join("lakefile.lean"))?;
    chain.write("lakefile.lean", &format!("{lakefile}-- edited\n"))?;
    let read = |file: &'static str| {
        let ctx = ctx.clone();
        tokio::spawn(async move { imports(&ctx, &[file]).await })
    };
    let (first, second) = (read("Chain/C.lean"), read("Chain/B.lean"));
    for result in [first.await?, second.await?] {
        match result {
            Ok(_) => {}
            // Lake refuses a second run that finds the same stale cache in the
            // same moment, rather than wait; the next run finds the new cache.
            Err(ImportsError::File(failure)) if failure.error.message.contains("exclusive configuration lock") => {
                imports(&ctx, &["Chain/C.lean"])
                    .await
                    .map_err(|error| format!("{error:?}"))?;
            }
            Err(error) => return Err(format!("{error:?}").into()),
        }
    }
    // The reader wrote Lake's cache of the elaborated lakefile.
    assert!(fs::metadata(&trace)?.modified()? > elaborated);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn imports_list_the_whole_closure_with_first_party_sources() -> TestResult {
    let (chain, ctx) = project()?;
    let reply = imports(&ctx, &["Chain/C.lean"])
        .await
        .map_err(|error| format!("{error:?}"))?;
    let [file] = reply.files.as_slice() else {
        return Err(format!("expected one file: {reply:?}").into());
    };
    assert_eq!(file.file, "Chain/C.lean");
    assert_eq!(file.module, "Chain.C");
    let import = |module: &str| {
        file.imports
            .iter()
            .find(|import| import.module == module)
            .ok_or_else(|| format!("C lists no {module}: {reply:?}"))
    };
    let path = |relative: &str| chain.root.join(relative).display().to_string();
    let b = import("Chain.B")?;
    assert_eq!(b.olean, chain.olean("Chain.B").display().to_string());
    assert_eq!(b.source, Some(path("Chain/B.lean")));
    // C imports only B, so A and Dep are there because the list is closed.
    let a = import("Chain.A")?;
    assert_eq!(a.source, Some(path("Chain/A.lean")));
    let dep = import("Dep")?;
    assert_eq!(dep.olean, path("dep/.lake/build/lib/lean/Dep.olean"));
    assert_eq!(dep.source, None, "Dep's olean lies outside the root's .lake/build");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn imports_of_a_file_with_a_stale_import_are_an_error_naming_the_file() -> TestResult {
    let (chain, ctx) = project()?;
    chain.write(
        "Chain/B.lean",
        "import Chain.A\nimport Dep\n\ndef b : Nat := a + dep\ndef extra : Nat := 0\n",
    )?;
    let result = imports(&ctx, &["Chain/C.lean"]).await;
    let Err(ImportsError::File(failure)) = &result else {
        return Err(format!("expected a failure for the file: {result:?}").into());
    };
    assert_eq!(failure.error.code, "setup_file_failed");
    let message = &failure.error.message;
    assert!(message.starts_with("Chain/C.lean: "), "{message}");
    assert!(message.contains("out-of-date"), "Lake's own words: {message}");
    let result = imports(&ctx, &["Chain/Missing.lean"]).await;
    let Err(ImportsError::File(failure)) = &result else {
        return Err(format!("expected a failure for the missing file: {result:?}").into());
    };
    assert_eq!(failure.error.code, "invalid_request");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[serial(native_owners)]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT and a matching installed worker"]
async fn a_check_sent_during_a_build_waits_and_sees_what_it_built() -> TestResult {
    let (chain, ctx) = project()?;
    chain.write(
        "Chain/B.lean",
        "import Chain.A\nimport Dep\n\ndef b : Nat := a + dep\ndef newB : Nat := 3\n",
    )?;
    chain.write(
        "Chain/C.lean",
        "import Chain.B\n\ntheorem b_eq : b = 2 := rfl\nexample : newB = 3 := rfl\n",
    )?;
    let build = tokio::spawn({
        let ctx = ctx.clone();
        async move { as_call(lean_build(&ctx, recipe("slowb"))).await }
    });
    wait_for_log(&chain.root, "$ lake env sleep 2").await?;
    let check = diagnostics(&ctx, "Chain/C.lean").await?;
    let checked_at = epoch_millis()?;
    let build = build.await??;
    assert!(build.ok, "{build:?}");
    assert_clean(&check)?;
    let built_at = build.steps.last().ok_or("the build ran steps")?.finished_at_ms;
    assert!(checked_at >= built_at, "the check ended before the build did");
    ctx.broker.shutdown_all();
    Ok(())
}
