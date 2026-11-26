//! Named requests over HTTP: one row and one launch per id, and the original
//! result for any client, after a dropped connection or a server kill.
//!
//! The tests without `#[ignore]` serve a Lake project with no Lean code; their
//! calls need no worker. The ignored tests build and check a chain project
//! (`support::chain`); set `LEAN_HOST_MCP_TEST_FIXTURE` or `LEAN_SYSROOT` to
//! run them, one at a time. The tests that check files also need
//! `LEAN_HOST_MCP_WORKERS_DIR` to name an installed worker.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TestResult;
use support::chain::{Chain, chain_project};
use support::http::{self, Client, Launch, Scratch, Server};

/// How long a test waits for a server to listen. A restart first waits for
/// the helpers of the server it replaces.
const STARTUP: Duration = Duration::from_mins(16);
/// How long a test waits for a request to end.
const PATIENCE: Duration = Duration::from_mins(5);

/// A call that needs no worker: a verification with no targets fails at once.
fn empty_verify() -> Value {
    json!({"targets": []})
}

/// A Lake project with no Lean code. Its manifest lists its packages, none,
/// as the read turn of a check needs.
fn metadata_project() -> TestResult<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("requests-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    std::fs::write(dir.path().join("lakefile.toml"), "name = \"requests\"\n")?;
    std::fs::write(dir.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n")?;
    std::fs::write(dir.path().join("lake-manifest.json"), "{\"packages\":[]}\n")?;
    Ok(dir)
}

/// The parts of a tool result that its request stores.
fn stored_part(result: &Value) -> Value {
    json!({
        "content": result.get("content"),
        "structuredContent": result.get("structuredContent"),
        "isError": result.get("isError"),
    })
}

/// The ids in the request table of `store`.
fn table_ids(store: &Path) -> TestResult<Vec<String>> {
    Ok(http::table(store)?.into_iter().map(|row| row.id).collect())
}

#[tokio::test]
async fn a_named_request_runs_once_and_keeps_its_result() -> TestResult {
    let project = metadata_project()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(project.path(), &scratch)?, STARTUP).await?;
    let first = Client::new(&server.url, "first")?;
    let id = "verify-empty-0001";
    let row = first.run(id, "lean_verify", &empty_verify()).await?;
    assert_eq!(row.get("request_id"), Some(&json!(id)), "{row}");
    assert_eq!(row.get("kind"), Some(&json!("check")), "{row}");

    // Another client reads the result the request stored.
    let ended = Client::new(&server.url, "second")?.ended(id, PATIENCE).await?;
    assert_eq!(http::state(&ended), Some("done"), "{ended}");
    let result = ended.get("result").ok_or("no result")?;
    assert_eq!(
        result.pointer("/structuredContent/request_id"),
        Some(&json!(id)),
        "{result}"
    );
    assert!(
        result
            .pointer("/structuredContent/errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty()),
        "a verification with no targets fails: {result}"
    );

    // The same call again returns the same row.
    let again = first.run(id, "lean_verify", &empty_verify()).await?;
    assert_eq!(again.get("created_at"), row.get("created_at"), "{again}");
    assert_eq!(again.get("state"), Some(&json!("done")), "{again}");

    // A direct call with the id returns the stored result.
    let direct = first
        .call("lean_verify", &json!({"targets": [], "request_id": id}))
        .await?;
    assert_eq!(stored_part(&direct), stored_part(result));

    // A direct call without an id runs under a server id and makes no row.
    let unnamed = first.send("lean_verify", &empty_verify()).await?;
    let unnamed_id = http::reply_id(&unnamed).ok_or("the reply names no id")?;
    assert!(unnamed_id.starts_with("srv-"), "{unnamed}");
    assert_eq!(table_ids(&scratch.store)?, [id]);

    let status = first.control(&json!({"action": "status"})).await?;
    assert_eq!(
        status.get("requests"),
        Some(&json!({"queued": 0, "running": 0})),
        "{status}"
    );
    assert!(
        status
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|capabilities| capabilities.contains(&json!("durable_requests_v1"))),
        "{status}"
    );
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn another_call_on_an_id_fails_and_makes_no_row() -> TestResult {
    let project = metadata_project()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(project.path(), &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "conflict")?;
    let id = "conflict-0001";
    client.run(id, "lean_verify", &empty_verify()).await?;

    // Another tool, other arguments, or a direct call: each names the call
    // the id holds and changes nothing.
    for (tool, arguments) in [
        ("lean_status", json!({"kind": "project"})),
        ("lean_verify", json!({"targets": [], "report_axioms": true})),
    ] {
        let refused = client.run_result(id, tool, &arguments).await?;
        assert_eq!(http::error_code(&refused), Some("request_id_conflict"), "{refused}");
    }
    let direct = client
        .send("lean_status", &json!({"kind": "project", "request_id": id}))
        .await?;
    assert_eq!(
        direct.pointer("/result/structuredContent/error/code"),
        Some(&json!("request_id_conflict")),
        "{direct}"
    );

    // Refusals that make no row.
    for (request_id, tool, arguments, code) in [
        ("unknown-tool-0001", "lean_nothing", json!({}), "unknown_tool"),
        ("nested-get-0001", "lean_get", json!({"request_id": id}), "unknown_tool"),
        // `lean_verify` reads its arguments only when it runs, so the bad
        // arguments go to a tool that decodes them first.
        (
            "bad-arguments-0001",
            "lean_imports",
            json!({"files": 7}),
            "invalid_arguments",
        ),
        (
            "inner-id-0001",
            "lean_verify",
            json!({"targets": [], "request_id": "inner-id-0001"}),
            "invalid_arguments",
        ),
        ("short", "lean_verify", empty_verify(), "invalid_request_id"),
        ("srv-mine-0001", "lean_verify", empty_verify(), "invalid_request_id"),
    ] {
        let refused = client.run_result(request_id, tool, &arguments).await?;
        assert_eq!(http::error_code(&refused), Some(code), "{request_id}: {refused}");
    }
    let bad_id = client
        .send("lean_verify", &json!({"targets": [], "request_id": "bad id!"}))
        .await?;
    assert_eq!(
        bad_id.pointer("/result/structuredContent/error/code"),
        Some(&json!("invalid_request_id")),
        "{bad_id}"
    );
    assert!(
        http::reply_id(&bad_id).is_some_and(|id| id.starts_with("srv-")),
        "{bad_id}"
    );
    let unknown = client
        .send("lean_nothing", &json!({"request_id": "direct-unknown-0001"}))
        .await?;
    assert_eq!(
        unknown.pointer("/error/data/request_id"),
        Some(&json!("direct-unknown-0001")),
        "{unknown}"
    );
    let missing = client.call("lean_get", &json!({"request_id": "missing-0001"})).await?;
    assert_eq!(http::error_code(&missing), Some("request_not_found"), "{missing}");

    assert_eq!(table_ids(&scratch.store)?, [id]);
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn an_ended_request_survives_a_server_kill() -> TestResult {
    let project = metadata_project()?;
    let scratch = Scratch::new()?;
    let launch = Launch::new(project.path(), &scratch)?;
    let server = Server::start(&launch, STARTUP).await?;
    let id = "survivor-0001";
    let client = Client::new(&server.url, "before")?;
    client.run(id, "lean_verify", &empty_verify()).await?;
    let before = client.ended(id, PATIENCE).await?;
    server.kill().await?;

    let server = Server::start(&launch, STARTUP).await?;
    let client = Client::new(&server.url, "after")?;
    assert_eq!(client.get(id, 0).await?, before);
    let row = client.run(id, "lean_verify", &empty_verify()).await?;
    assert_eq!(Some(&row), before.get("request"));
    assert_eq!(table_ids(&scratch.store)?, [id]);
    server.stop().await?;
    Ok(())
}

/// The recipe the build tests run: it adds its first argument as a line to
/// `marker.txt` in the root, then sleeps for its second, in seconds.
const MARK: &str = r#"
[recipes.mark]
steps = [["env", "sh", "-c", "echo \"$1\" >> marker.txt; sleep \"$2\"", "mark"]]
append = "last"
"#;

/// A built chain project with the `mark` recipe.
fn marked_chain() -> TestResult<Chain> {
    let chain = chain_project()?;
    chain.write("lean-host-mcp.toml", MARK)?;
    Ok(chain)
}

/// `lean_build` arguments that run `mark` with `tag`, sleeping `seconds`.
fn mark(tag: &str, seconds: u32) -> Value {
    json!({"recipe": "mark", "args": [tag, seconds.to_string()]})
}

/// The lines of `marker.txt` in `root`: one per build that reached its step.
fn marker_lines(root: &Path) -> TestResult<Vec<String>> {
    match std::fs::read_to_string(root.join("marker.txt")) {
        Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Wait until a build with `tag` has reached its step.
async fn wait_for_marker(root: &Path, tag: &str) -> TestResult {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    while !marker_lines(root)?.iter().any(|line| line == tag) {
        if Instant::now() >= until {
            return Err(format!("no build marked {tag} within {PATIENCE:?}").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

/// Assert that the build of a `lean_get` reply ran and passed.
fn assert_passed(reply: &Value) {
    assert_eq!(http::state(reply), Some("done"), "{reply}");
    assert_eq!(reply.pointer("/request/exit_code"), Some(&json!(0)), "{reply}");
    assert_eq!(reply.pointer("/result/isError"), Some(&json!(false)), "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn the_same_build_sent_three_times_at_once_starts_one_lake() -> TestResult {
    let chain = marked_chain()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let id = "twice-build-0001";
    let arguments = mark("once", 1);
    let (one, two, three) = (
        Client::new(&server.url, "one")?,
        Client::new(&server.url, "two")?,
        Client::new(&server.url, "three")?,
    );
    let mut direct = arguments.clone();
    direct
        .as_object_mut()
        .ok_or("arguments")?
        .insert("request_id".to_owned(), json!(id));
    let (first, second, waited) = tokio::join!(
        one.run(id, "lean_build", &arguments),
        two.run(id, "lean_build", &arguments),
        three.call("lean_build", &direct),
    );
    let (first, second, waited) = (first?, second?, waited?);
    assert_eq!(first.get("created_at"), second.get("created_at"), "{first} {second}");
    let ended = one.ended(id, PATIENCE).await?;
    assert_passed(&ended);
    assert_eq!(
        Some(&stored_part(&waited)),
        ended.get("result").map(stored_part).as_ref()
    );
    assert_eq!(marker_lines(&chain.root)?, ["once"]);
    assert_eq!(table_ids(&scratch.store)?, [id]);
    server.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_build_outlives_its_killed_client() -> TestResult {
    let chain = marked_chain()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let id = "orphan-build-0001";
    let mut arguments = mark("orphan", 3);
    arguments
        .as_object_mut()
        .ok_or("arguments")?
        .insert("request_id".to_owned(), json!(id));
    let mut doomed = Client::new(&server.url, "doomed")?.process("lean_build", &arguments)?;
    wait_for_marker(&chain.root, "orphan").await?;
    doomed.start_kill()?;
    doomed.wait().await?;

    let ended = Client::new(&server.url, "other")?.ended(id, PATIENCE).await?;
    assert_passed(&ended);
    assert_eq!(marker_lines(&chain.root)?, ["orphan"]);
    server.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn a_killed_server_interrupts_its_build_and_runs_the_queued_one() -> TestResult {
    let chain = marked_chain()?;
    let scratch = Scratch::new()?;
    let launch = Launch::new(&chain.root, &scratch)?;
    let server = Server::start(&launch, STARTUP).await?;
    let client = Client::new(&server.url, "before")?;
    let (killed, queued) = ("killed-build-0001", "queued-build-0001");
    client.run(killed, "lean_build", &mark("killed", 20)).await?;
    wait_for_marker(&chain.root, "killed").await?;
    let row = client.run(queued, "lean_build", &mark("queued", 1)).await?;
    assert_eq!(row.get("state"), Some(&json!("queued")), "{row}");

    let tree = http::descendants(server.pid()?)?;
    server.kill().await?;
    let killed_at = Instant::now();
    let report = std::thread::spawn(move || http::watch_until_gone(&tree, killed_at, Duration::from_mins(6)));
    let server = Server::start(&launch, STARTUP).await?;
    eprintln!("the restart listened {:?} after the kill", killed_at.elapsed());
    for line in server.log().lines().filter(|line| line.contains("earlier server")) {
        eprintln!("recovery: {line}");
    }

    let client = Client::new(&server.url, "after")?;
    let interrupted = client.get(killed, 0).await?;
    assert_eq!(http::state(&interrupted), Some("interrupted"), "{interrupted}");
    assert_eq!(interrupted.get("result"), Some(&Value::Null), "{interrupted}");
    assert_passed(&client.ended(queued, PATIENCE).await?);
    // A retry returns the interrupted row and starts nothing.
    let retry = client.run(killed, "lean_build", &mark("killed", 20)).await?;
    assert_eq!(retry.get("state"), Some(&json!("interrupted")), "{retry}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(marker_lines(&chain.root)?, ["killed", "queued"]);
    server.stop().await?;
    for line in report.join().map_err(|_| "the process watch failed")? {
        eprintln!("after the kill: {line}");
    }
    Ok(())
}

/// A file whose check holds Lean for two seconds.
const SLOW: &str = "#eval IO.sleep 2000\n";

/// The start and end of the row in a `lean_get` reply.
fn span(reply: &Value) -> TestResult<(i64, i64)> {
    let at = |field: &str| {
        reply
            .pointer(&format!("/request/{field}"))
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("no {field}: {reply}"))
    };
    Ok((at("started_at")?, at("finished_at")?))
}

/// Two checks on one project run at once: each takes the shared read turn and
/// marks its row `running`, so neither row waits for the other to end. Their
/// Lean processes do not overlap: each file check takes the SDK's one native
/// preparation slot in turn. Each check holds Lean for two seconds, so the
/// rows must overlap by more than one; with one turn at a time they would
/// overlap by a few milliseconds at most. File checks run Lean from an
/// installed worker's shim deployment, and the worker refuses the project
/// commands a worker check would need.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT, and LEAN_HOST_MCP_WORKERS_DIR"]
async fn checks_on_one_project_run_at_once() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = chain_project()?;
    let files = ["SlowOne.lean", "SlowTwo.lean"];
    for file in files {
        std::fs::write(chain.root.join(file), SLOW)?;
    }
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "checks")?;
    let ids = ["overlap-one-0001", "overlap-two-0001"];
    for (id, file) in ids.into_iter().zip(files) {
        client
            .run(id, "lean_status", &json!({"kind": "file_diagnostics", "file": file}))
            .await?;
    }
    let mut spans = Vec::new();
    for id in ids {
        let reply = client.ended(id, PATIENCE).await?;
        assert_eq!(http::state(&reply), Some("done"), "{reply}");
        assert_eq!(reply.pointer("/result/isError"), Some(&json!(false)), "{reply}");
        spans.push(span(&reply)?);
    }
    let [first, second] = spans[..] else {
        return Err("expected two rows".into());
    };
    let overlap = first.1.min(second.1).saturating_sub(first.0.max(second.0));
    eprintln!("the check rows overlapped by {overlap} ms: {first:?} {second:?}");
    assert!(
        overlap > 1000,
        "the checks were not running at once: {first:?} {second:?}"
    );
    server.stop().await?;
    Ok(())
}

/// A direct `lean_verify` with no request id works, names a server id and the
/// digest of the text it read. File checks run Lean from an installed
/// worker's shim deployment, so `LEAN_HOST_MCP_WORKERS_DIR` must name one, as
/// `lean-host-mcp install-worker --workers-dir` writes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT, and LEAN_HOST_MCP_WORKERS_DIR"]
async fn a_direct_verify_names_a_server_id() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = chain_project()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "verify")?;
    let verify = client
        .send(
            "lean_verify",
            &json!({"targets": [{"kind": "explicit", "file": "Chain/C.lean", "declarations": ["b_eq"]}]}),
        )
        .await?;
    assert_eq!(verify.pointer("/result/isError"), Some(&json!(false)), "{verify}");
    assert!(
        http::reply_id(&verify).is_some_and(|id| id.starts_with("srv-")),
        "{verify}"
    );
    let text = std::fs::read(chain.root.join("Chain/C.lean"))?;
    assert_eq!(
        verify
            .pointer("/result/structuredContent/trust/source_sha256")
            .and_then(Value::as_str),
        Some(http::sha256_hex(&text).as_str()),
        "{verify}"
    );
    assert!(table_ids(&scratch.store)?.is_empty(), "a direct call made a row");
    server.stop().await?;
    Ok(())
}
