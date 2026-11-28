//! Named requests over HTTP: one row and one launch per id, and the original
//! result for any client, after a dropped connection or a server kill.
//!
//! The tests without `#[ignore]` serve a Lake project with no Lean code; their
//! calls need no worker. The ignored tests build and check a chain project
//! (`support::chain`); set `LEAN_HOST_MCP_TEST_FIXTURE` or `LEAN_SYSROOT` to
//! run them, one at a time. The tests that check files also need
//! `LEAN_HOST_MCP_WORKERS_DIR` to name an installed worker.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lean_host_mcp_test_support as support;
use serde_json::{Value, json};
use support::TestResult;
use support::chain::{Chain, chain_project};
use support::http::{self, Client, Launch, Process, Scratch, Server};

/// The server binary Cargo built for this test run.
const SERVER: &str = env!("CARGO_BIN_EXE_lean-host-mcp");
/// The directory Cargo gives this suite for scratch files.
const TMP: &str = env!("CARGO_TARGET_TMPDIR");
/// How long a test waits for a server to listen. A restart first stops what
/// the server it replaces left running, which takes up to 35 seconds.
const STARTUP: Duration = Duration::from_mins(2);
/// How long a test waits for a request to end.
const PATIENCE: Duration = Duration::from_mins(5);

/// A call that needs no worker: a verification with no targets fails at once.
fn empty_verify() -> Value {
    json!({"targets": []})
}

/// A Lake project with no Lean code. Its manifest lists its packages, none,
/// as the read turn of a check needs.
fn metadata_project() -> TestResult<tempfile::TempDir> {
    let dir = tempfile::Builder::new().prefix("requests-").tempdir_in(TMP)?;
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
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, project.path(), &scratch)?, STARTUP).await?;
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

    let status = first.runtime().await?;
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
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, project.path(), &scratch)?, STARTUP).await?;
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
    let scratch = Scratch::new(TMP)?;
    let launch = Launch::new(SERVER, project.path(), &scratch)?;
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
    let chain = chain_project(TMP)?;
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

/// Wait until `path` exists.
async fn wait_for_path(path: &Path) -> TestResult {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    while !path.exists() {
        if Instant::now() >= until {
            return Err(format!("{} did not appear within {PATIENCE:?}", path.display()).into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
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
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, &chain.root, &scratch)?, STARTUP).await?;
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
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, &chain.root, &scratch)?, STARTUP).await?;
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
    let scratch = Scratch::new(TMP)?;
    let launch = Launch::new(SERVER, &chain.root, &scratch)?;
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

/// A recipe whose one step ignores SIGTERM, writes `stuck.txt` in the root,
/// then sleeps for ten minutes. Lake runs the shell, which passes on its
/// ignored SIGTERM to `sleep`.
const STUCK: &str = r#"
[recipes.stuck]
steps = [["env", "sh", "-c", "trap '' TERM; echo stuck > stuck.txt; sleep 600"]]
"#;

/// Whether server log `log` says the server sent `signal` to process group
/// `group`.
fn signalled(log: &str, group: u32, signal: i32) -> bool {
    log.lines().any(|line| {
        line.contains("signalled a process group of an earlier server")
            && line.split_whitespace().any(|word| word == format!("group={group}"))
            && line.split_whitespace().any(|word| word == format!("signal={signal}"))
    })
}

/// A server killed during a build leaves the build step's process group
/// running, and this group ignores SIGTERM. The next server on the same store
/// sends the group SIGTERM, then SIGKILL once its grace period ends, and
/// listens only after the group has exited: a `ps` sample every 20 ms saw the
/// group last before the server listened. Then the server serves: the killed
/// build's row is `interrupted`, and a new build runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT"]
async fn the_next_server_serves_only_after_a_killed_servers_build_exits() -> TestResult {
    let chain = chain_project(TMP)?;
    chain.write("lean-host-mcp.toml", &format!("{MARK}{STUCK}"))?;
    let scratch = Scratch::new(TMP)?;
    let launch = Launch::new(SERVER, &chain.root, &scratch)?;
    let server = Server::start(&launch, STARTUP).await?;
    let client = Client::new(&server.url, "before")?;
    let id = "stuck-build-0001";
    client.run(id, "lean_build", &json!({"recipe": "stuck"})).await?;
    wait_for_path(&chain.root.join("stuck.txt")).await?;
    let all = http::processes()?;
    let group = match http::lake_builds(&all, &scratch.store)[..] {
        [build] => build.pgid,
        ref builds => return Err(format!("expected one Lake build step: {builds:?}").into()),
    };
    let in_group = move |all: &[Process]| all.iter().filter(|process| process.pgid == group).count();
    server.kill().await?;
    let killed_at = Instant::now();
    assert!(
        in_group(&http::processes()?) > 0,
        "the build step's group exited with its server"
    );
    // The time before the last sample that saw the group.
    let watch = std::thread::spawn(move || -> Result<Instant, String> {
        let mut seen = killed_at;
        loop {
            let at = Instant::now();
            let all = http::processes().map_err(|error| error.to_string())?;
            if in_group(&all) == 0 {
                return Ok(seen);
            }
            seen = at;
            if killed_at.elapsed() > Duration::from_mins(3) {
                return Err(format!("group {group} still runs 3 minutes after the kill"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let restarted = Instant::now();
    let server = Server::start(&launch, STARTUP).await?;
    let listened = Instant::now();
    let seen = watch.join().map_err(|_| "the group watch failed")??;
    eprintln!(
        "the next server listened {:?} after it started; a sample saw the group {:?} before that",
        listened.duration_since(restarted),
        listened.duration_since(seen)
    );
    assert!(seen < listened, "the next server listened while the group ran");
    let log = server.log();
    assert!(signalled(&log, group, 15), "no SIGTERM to group {group}:\n{log}");
    assert!(signalled(&log, group, 9), "no SIGKILL to group {group}:\n{log}");

    let client = Client::new(&server.url, "after")?;
    let interrupted = client.get(id, 0).await?;
    assert_eq!(http::state(&interrupted), Some("interrupted"), "{interrupted}");
    client.run("after-mark-0001", "lean_build", &mark("after", 0)).await?;
    assert_passed(&client.ended("after-mark-0001", PATIENCE).await?);
    server.stop().await?;
    Ok(())
}

/// A server killed while its worker is warm and a file check runs: the worker
/// exits with its server, the check's Lean process outlives it, and the next
/// server stops that process before it listens. Its build then runs at once.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT, and LEAN_HOST_MCP_WORKERS_DIR"]
async fn a_killed_servers_workers_exit_and_the_next_build_runs() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = marked_chain()?;
    chain.write("Slow.lean", "#eval IO.sleep 60000\n")?;
    let scratch = Scratch::new(TMP)?;
    let launch = Launch::new(SERVER, &chain.root, &scratch)?;
    let server = Server::start(&launch, STARTUP).await?;
    let client = Client::new(&server.url, "warm")?;
    let trial = json!({"kind": "command", "imports": ["Chain.C"], "commands": "#check b_eq"});
    client.run("warm-trial-0001", "lean_trial", &trial).await?;
    let ended = client.ended("warm-trial-0001", PATIENCE).await?;
    assert_eq!(http::state(&ended), Some("done"), "{ended}");
    let check = json!({"kind": "file_diagnostics", "file": "Slow.lean"});
    client.run("slow-check-0001", "lean_status", &check).await?;
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    let lean = |command: &str| {
        command.contains("Slow.lean")
            && command
                .split_whitespace()
                .next()
                .is_some_and(|program| program.ends_with("/lean"))
    };
    let tree = loop {
        let tree = http::descendants(server.pid()?)?;
        if tree.iter().any(|process| lean(&process.command)) {
            break tree;
        }
        if Instant::now() >= until {
            return Err("no Lean process checks Slow.lean".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let workers: Vec<Process> = http::workers(&tree, server.pid()?).into_iter().cloned().collect();
    assert!(!workers.is_empty(), "no warm worker: {tree:#?}");
    let checks: Vec<Process> = tree.iter().filter(|process| lean(&process.command)).cloned().collect();
    server.kill().await?;
    let killed_at = Instant::now();
    let report = http::watch_until_gone(&workers, killed_at, Duration::from_secs(30));
    for line in &report {
        eprintln!("after the kill: {line}");
    }
    let left: Vec<&String> = report.iter().filter(|line| line.contains("still running")).collect();
    assert!(left.is_empty(), "30 s after the kill: {left:#?}");
    let running = |checks: &[Process]| -> TestResult<bool> {
        let live = http::processes()?;
        Ok(checks
            .iter()
            .any(|check| live.iter().any(|process| process.pid == check.pid)))
    };
    assert!(running(&checks)?, "the check's Lean process exited with its server");

    let server = Server::start(&launch, STARTUP).await?;
    assert!(!running(&checks)?, "the check still ran when the next server listened");
    let client = Client::new(&server.url, "after")?;
    let started = Instant::now();
    client.run("after-build-0001", "lean_build", &mark("after", 0)).await?;
    assert_passed(&client.ended("after-build-0001", PATIENCE).await?);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the build took {:?}",
        started.elapsed()
    );
    server.stop().await?;
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

/// The files [`checks_at_once`] checks, each with [`SLOW`].
const SLOW_FILES: [&str; 2] = ["SlowOne.lean", "SlowTwo.lean"];

/// Whether `process` is a Lean process that checks `file`.
fn checks_file(process: &Process, file: &str) -> bool {
    process.command.contains(file)
        && process
            .command
            .split_whitespace()
            .next()
            .is_some_and(|program| program.ends_with("/lean"))
}

/// When a `ps` sample every 20 ms saw a Lean process check each of
/// [`SLOW_FILES`], in milliseconds from the first call.
#[derive(Debug)]
struct LeanRuns {
    /// For each file, the first and last sample that saw its Lean process.
    spans: [Option<(u128, u128)>; 2],
    /// The samples that saw both files' Lean processes at once.
    together: u32,
}

impl LeanRuns {
    /// How long the samples saw both Lean processes run at once, in
    /// milliseconds: zero when one ended before the other started.
    fn overlap(&self) -> TestResult<u128> {
        let [Some(one), Some(two)] = self.spans else {
            return Err(format!("a Lean process was never seen: {self:?}").into());
        };
        Ok(one.1.min(two.1).saturating_sub(one.0.max(two.0)))
    }
}

/// Check each of [`SLOW_FILES`] as a named request on `server`, each from its
/// own client and both at once, while a thread lists the processes every
/// 20 ms. Returns each row's start and end and what the thread saw.
async fn checks_at_once(server: &Server) -> TestResult<(Vec<(i64, i64)>, LeanRuns)> {
    let stop = Arc::new(AtomicBool::new(false));
    let since = Instant::now();
    let sampler = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut runs = LeanRuns {
                spans: [None; 2],
                together: 0,
            };
            while !stop.load(Ordering::SeqCst) {
                let at = since.elapsed().as_millis();
                if let Ok(all) = http::processes() {
                    let seen = SLOW_FILES.map(|file| all.iter().any(|process| checks_file(process, file)));
                    for (span, seen) in runs.spans.iter_mut().zip(seen) {
                        if seen {
                            *span = Some(span.map_or((at, at), |(first, _)| (first, at)));
                        }
                    }
                    if seen.iter().all(|seen| *seen) {
                        runs.together = runs.together.saturating_add(1);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            runs
        }
    });
    let clients = [
        Client::new(&server.url, "checks-one")?,
        Client::new(&server.url, "checks-two")?,
    ];
    let ids = ["lean-runs-one-0001", "lean-runs-two-0001"];
    for ((client, id), file) in clients.iter().zip(ids).zip(SLOW_FILES) {
        client
            .run(id, "lean_status", &json!({"kind": "file_diagnostics", "file": file}))
            .await?;
    }
    let mut spans = Vec::new();
    for (client, id) in clients.iter().zip(ids) {
        let reply = client.ended(id, PATIENCE).await?;
        assert_eq!(http::state(&reply), Some("done"), "{reply}");
        assert_eq!(reply.pointer("/result/isError"), Some(&json!(false)), "{reply}");
        spans.push(span(&reply)?);
    }
    stop.store(true, Ordering::SeqCst);
    let runs = sampler.join().map_err(|_| "the process sampler failed")?;
    eprintln!("check rows (start, end) in ms since the epoch: {spans:?}");
    eprintln!("Lean processes (first, last seen) in ms after the first call: {runs:?}");
    Ok((spans, runs))
}

/// A chain project with [`SLOW_FILES`].
fn slow_chain() -> TestResult<Chain> {
    let chain = chain_project(TMP)?;
    for file in SLOW_FILES {
        chain.write(file, SLOW)?;
    }
    Ok(chain)
}

/// Two file checks on one project, sent at once from two clients, run Lean at
/// once under the default `broker.max_lean_runs` of 2: each Lean process holds
/// Lean for two seconds, and the samples see both run together for more than
/// one second.
/// File checks need an installed worker's shim deployment.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT, and LEAN_HOST_MCP_WORKERS_DIR"]
async fn two_file_checks_run_lean_at_once() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = slow_chain()?;
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, &chain.root, &scratch)?, STARTUP).await?;
    let (_, runs) = checks_at_once(&server).await?;
    let overlap = runs.overlap()?;
    eprintln!("the Lean processes ran at once for {overlap} ms");
    assert!(runs.together > 0, "no sample saw both Lean processes: {runs:?}");
    assert!(overlap > 1000, "the Lean processes barely overlapped: {runs:?}");
    server.stop().await?;
    Ok(())
}

/// With `broker.max_lean_runs` at 1, the same two checks take turns: no
/// sample sees both Lean processes, and the checks take more than the four
/// seconds the two Lean processes hold Lean in turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker: set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT, and LEAN_HOST_MCP_WORKERS_DIR"]
async fn with_one_lean_run_two_file_checks_take_turns() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = slow_chain()?;
    let scratch = Scratch::new(TMP)?;
    let launch = Launch::new(SERVER, &chain.root, &scratch)?.env("LEAN_HOST_MCP_MAX_LEAN_RUNS", "1");
    let server = Server::start(&launch, STARTUP).await?;
    let (spans, runs) = checks_at_once(&server).await?;
    assert_eq!(runs.together, 0, "a sample saw both Lean processes: {runs:?}");
    assert_eq!(runs.overlap()?, 0, "the Lean processes overlapped: {runs:?}");
    let start = spans.iter().map(|span| span.0).min().ok_or("no rows")?;
    let end = spans.iter().map(|span| span.1).max().ok_or("no rows")?;
    let total = end.saturating_sub(start);
    eprintln!("the two checks took {total} ms from the first start to the last end");
    assert!(total > 4000, "the checks took {total} ms: {spans:?}");
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
    let chain = chain_project(TMP)?;
    let scratch = Scratch::new(TMP)?;
    let server = Server::start(&Launch::new(SERVER, &chain.root, &scratch)?, STARTUP).await?;
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
