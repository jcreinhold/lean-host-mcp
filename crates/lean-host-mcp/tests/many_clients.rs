//! Twelve clients on one project through `lean_run` and `lean_get`: eight
//! check files, half of them with a command trial in their second round, and
//! four build, while the test edits a file during a build, kills a client, and
//! kills the server during a build and starts it again.
//!
//! The test asserts that Lake writer steps never overlap, that a `ps` sampler
//! every 50 ms sees at most one Lake writer, that each ended build row wrote
//! one marker line, that every check reply's `source_sha256` matches the text
//! it read and no check reports a missing import, that every trial prints
//! what its commands should, that the worker count stays within its limit,
//! and that every row ends.
//!
//! It is ignored: it needs a Lean toolchain and an installed worker, whose
//! shim deployment file checks run Lean from. Set `LEAN_HOST_MCP_TEST_FIXTURE`
//! or `LEAN_SYSROOT`, and `LEAN_HOST_MCP_WORKERS_DIR` to a directory that
//! `lean-host-mcp install-worker --workers-dir` wrote, to run it on a chain
//! project (`support::chain`). To run the same clients against a running server, with
//! no edits or kills, set `LEAN_HOST_MCP_MANY_CLIENTS_URL` to its endpoint,
//! `LEAN_HOST_MCP_MANY_CLIENTS_ROOT` to its Lake root, and
//! `LEAN_HOST_MCP_MANY_CLIENTS_FILES` and `LEAN_HOST_MCP_MANY_CLIENTS_MODULES`
//! to the files to check and the modules to build, separated by commas.

#![cfg(unix)]

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use support::TestResult;
use support::chain::chain_project;
use support::http::{self, Client, Launch, Scratch, Server};
use tokio::task::JoinHandle;

const CHECK_CLIENTS: usize = 8;
const BUILD_CLIENTS: usize = 4;
/// Requests each client sends, one after another.
const ROUNDS: usize = 3;
/// The worker limit of the fixture server.
const MAX_WORKERS: usize = 2;
const SAMPLE_EVERY: Duration = Duration::from_millis(50);
/// How long clients keep trying while the server is down, and how long a
/// restart may take: recovery first waits for the killed server's helpers.
const RESTART: Duration = Duration::from_mins(16);
/// How long a client waits for one request to end.
const PATIENCE: Duration = Duration::from_mins(30);

/// The files the fixture's check clients read.
const CHECKED: [&str; 2] = ["Chain/C.lean", "Chain/B.lean"];
/// The file the test edits during a build, and its new text.
const EDITED: (&str, &str) = (
    "Chain/C.lean",
    "import Chain.B\n\ntheorem b_eq : b = 2 := rfl\n\ntheorem two_eq : 2 = b := rfl\n",
);
/// Each fixture build runs Lake's default targets, then adds its tag as a
/// line to `marker.txt` and sleeps two seconds.
const CONFIG: &str = r#"
[recipes.mark]
steps = [["build"], ["env", "sh", "-c", "echo \"$1\" >> marker.txt; sleep 2", "mark"]]
append = "last"
"#;

/// What the clients work on.
struct Work {
    url: String,
    root: PathBuf,
    files: Vec<String>,
    /// Modules to build; none on the fixture, whose builds run `mark`.
    modules: Vec<String>,
    /// The `lean_trial` arguments of a check client's trial.
    trial: Value,
    /// Text the trial's output must hold.
    printed: String,
    /// Starts every request id, so that runs against one server differ.
    prefix: String,
}

impl Work {
    /// The `lean_build` arguments of the build tagged `tag`, the `round`-th of
    /// its client.
    fn build(&self, tag: &str, round: usize) -> Value {
        match pick(&self.modules, round) {
            Some(module) => json!({"targets": [module]}),
            None => json!({"recipe": "mark", "args": [tag]}),
        }
    }
}

/// The `index`-th item of `items`, counting around.
fn pick(items: &[String], index: usize) -> Option<&String> {
    items.get(index.checked_rem(items.len())?)
}

/// What a request asked for.
#[derive(Debug)]
enum Kind {
    /// A file check: the file and the digests of its text before the call and
    /// after the reply.
    File(String, [String; 2]),
    /// A command trial.
    Trial,
    Build,
}

/// One request, as its client saw it end.
#[derive(Debug)]
struct Outcome {
    id: String,
    kind: Kind,
    /// The final `lean_get` reply: `{request, result}`.
    reply: Value,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
async fn many_clients_share_one_project() -> TestResult {
    match std::env::var("LEAN_HOST_MCP_MANY_CLIENTS_URL") {
        Ok(url) => on_running_server(url).await,
        Err(_) => on_fixture().await,
    }
}

async fn on_fixture() -> TestResult {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    let chain = chain_project()?;
    chain.write("lean-host-mcp.toml", CONFIG)?;
    let scratch = Scratch::new()?;
    let max = MAX_WORKERS.to_string();
    let launch = Launch::new(&chain.root, &scratch)?
        .env("LEAN_HOST_MCP_MAX_WORKERS", &max)
        .env("LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT", &max);
    let server = Server::start(&launch, RESTART).await?;
    let sampler = Sampler::start(&scratch.store, server.pid()?);
    let work = Arc::new(Work {
        url: launch.url(),
        root: chain.root.clone(),
        files: CHECKED.map(str::to_owned).to_vec(),
        modules: Vec::new(),
        trial: json!({"kind": "command", "imports": ["Chain.C"], "commands": "#check b_eq\n#print axioms b_eq"}),
        printed: "'b_eq' does not depend on any axioms".to_owned(),
        prefix: prefix(),
    });
    // Client 12 is a process the test kills during its build.
    let clients = spawn_clients(&work, BUILD_CLIENTS.saturating_sub(1))?;
    let killed_id = format!("{}-build-killed", work.prefix);
    let mut arguments = work.build(&killed_id, 0);
    arguments
        .as_object_mut()
        .ok_or("arguments")?
        .insert("request_id".to_owned(), json!(killed_id));
    let mut killed = Client::new(&work.url, "killed")?.process("lean_build", &arguments)?;

    wait_for_markers(&chain.root, 1).await?;
    replace(&chain.root.join(EDITED.0), EDITED.1)?;
    wait_for_marker(&chain.root, &killed_id).await?;
    killed.start_kill()?;
    killed.wait().await?;

    // Kill the server as a later build reaches its marker step.
    let seen = marker_lines(&chain.root)?.len();
    wait_for_markers(&chain.root, seen.saturating_add(1)).await?;
    let tree = http::descendants(server.pid()?)?;
    let first_log = server.kill().await?;
    let killed_at = Instant::now();
    sampler.server.store(0, Ordering::SeqCst);
    let watch = std::thread::spawn(move || http::watch_until_gone(&tree, killed_at, RESTART));
    let server = Server::start(&launch, RESTART).await?;
    let restart = killed_at.elapsed();
    sampler.server.store(server.pid()?, Ordering::SeqCst);

    let mut outcomes = collect(clients).await?;
    let after = Client::new(&work.url, "after")?;
    outcomes.push(Outcome {
        id: killed_id.clone(),
        kind: Kind::Build,
        reply: settle(&after, &killed_id).await?,
    });
    let sampled = sampler.stop()?;
    let rows = http::table(&scratch.store)?;
    let markers = marker_lines(&chain.root)?;
    let last_log = server.stop().await?;

    eprintln!("the restart listened {restart:?} after the kill");
    for line in first_log.lines().chain(last_log.lines()) {
        if ["earlier server", "interrupted", "queued request", " WARN ", " ERROR "]
            .iter()
            .any(|word| line.contains(word))
        {
            eprintln!("log: {line}");
        }
    }
    for line in watch.join().map_err(|_| "the process watch failed")? {
        eprintln!("after the kill: {line}");
    }
    report(&outcomes, &sampled);

    let killed_reply = outcomes
        .iter()
        .find(|outcome| outcome.id == killed_id)
        .map(|outcome| &outcome.reply)
        .ok_or("the killed client's request")?;
    assert_eq!(http::state(killed_reply), Some("done"), "{killed_reply}");
    // Every build holds Lake for two seconds and every check runs Lean, so a
    // sampler that saw neither would prove nothing.
    assert_eq!(
        sampled.most_writers, 1,
        "the sampler never saw a Lake writer: {sampled:?}"
    );
    assert!(sampled.most_workers > 0, "the sampler never saw a worker: {sampled:?}");
    assert_common(&outcomes, &sampled, MAX_WORKERS, &work.printed)?;
    let unended: Vec<_> = rows
        .iter()
        .filter(|row| !http::FINAL_STATES.contains(&row.state.as_str()))
        .collect();
    assert!(unended.is_empty(), "rows that never ended: {unended:?}");
    let ended_builds = rows
        .iter()
        .filter(|row| row.kind == "build" && http::FINAL_STATES.contains(&row.state.as_str()))
        .count();
    assert_eq!(markers.len(), ended_builds, "marker lines {markers:?} for {rows:?}");
    let distinct: BTreeSet<&String> = markers.iter().collect();
    assert_eq!(distinct.len(), markers.len(), "a build ran twice: {markers:?}");
    Ok(())
}

async fn on_running_server(url: String) -> TestResult {
    let variable = |name: &str| std::env::var(name).map_err(|_| format!("set {name}"));
    let list = |name: &str| -> TestResult<Vec<String>> {
        Ok(variable(name)?
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect())
    };
    let modules = list("LEAN_HOST_MCP_MANY_CLIENTS_MODULES")?;
    let work = Arc::new(Work {
        url,
        root: PathBuf::from(variable("LEAN_HOST_MCP_MANY_CLIENTS_ROOT")?).canonicalize()?,
        files: list("LEAN_HOST_MCP_MANY_CLIENTS_FILES")?,
        trial: json!({"kind": "command", "imports": modules, "commands": "#check Nat"}),
        printed: "Nat : Type".to_owned(),
        modules,
        prefix: prefix(),
    });
    if work.files.is_empty() || work.modules.is_empty() {
        return Err("name at least one file and one module".into());
    }
    // The sampler finds builds through the server's store: the default one.
    let sampler = Sampler::start(&lean_host_mcp::store::store_dir(&work.root, None), 0);
    let control = Client::new(&work.url, "pool")?;
    let stop = Arc::new(AtomicBool::new(false));
    let watched = Arc::clone(&stop);
    let pool = tokio::spawn(async move { watch_pool(control, watched).await.map_err(|error| error.to_string()) });
    let clients = spawn_clients(&work, BUILD_CLIENTS)?;
    let outcomes = collect(clients).await?;
    stop.store(true, Ordering::SeqCst);
    let (most, limit) = pool.await??;
    let sampled = sampler.stop()?;
    report(&outcomes, &sampled);
    eprintln!("the server's pool held at most {most} of {limit} workers");
    assert!(most <= limit, "the pool held {most} workers, over its limit {limit}");
    assert_common(&outcomes, &sampled, usize::MAX, &work.printed)
}

/// A request id prefix of its own for this run.
fn prefix() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("mc-{}", id.get(..8).unwrap_or(&id))
}

/// What a client task returns. A boxed error is not `Send`, so a task returns
/// its message.
type Sent<T> = Result<T, String>;

/// Start the check clients and `builders` build clients.
fn spawn_clients(work: &Arc<Work>, builders: usize) -> TestResult<Vec<JoinHandle<Sent<Vec<Outcome>>>>> {
    let mut clients = Vec::new();
    for number in 0..CHECK_CLIENTS {
        let client = Client::new(&work.url, &format!("check-{number}"))?;
        let work = Arc::clone(work);
        clients.push(tokio::spawn(async move {
            check_client(client, work, number)
                .await
                .map_err(|error| error.to_string())
        }));
    }
    for number in 0..builders {
        let client = Client::new(&work.url, &format!("build-{number}"))?;
        let work = Arc::clone(work);
        clients.push(tokio::spawn(async move {
            build_client(client, work, number)
                .await
                .map_err(|error| error.to_string())
        }));
    }
    Ok(clients)
}

/// Wait for every client and gather what they saw.
async fn collect(clients: Vec<JoinHandle<Sent<Vec<Outcome>>>>) -> TestResult<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for client in clients {
        outcomes.extend(client.await??);
    }
    Ok(outcomes)
}

async fn check_client(client: Client, work: Arc<Work>, number: usize) -> TestResult<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for round in 0..ROUNDS {
        let id = format!("{}-check-{number}-{round}", work.prefix);
        if round == 1 && number.is_multiple_of(2) {
            run(&client, &id, "lean_trial", &work.trial).await?;
            let reply = settle(&client, &id).await?;
            outcomes.push(Outcome {
                id,
                kind: Kind::Trial,
                reply,
            });
            continue;
        }
        let file = pick(&work.files, number.saturating_add(round)).ok_or("no file to check")?;
        let path = work.root.join(file);
        let before = http::sha256_hex(&std::fs::read(&path)?);
        let arguments = json!({"kind": "file_diagnostics", "file": file});
        run(&client, &id, "lean_status", &arguments).await?;
        let reply = settle(&client, &id).await?;
        let after = http::sha256_hex(&std::fs::read(&path)?);
        outcomes.push(Outcome {
            id,
            kind: Kind::File(file.clone(), [before, after]),
            reply,
        });
    }
    Ok(outcomes)
}

async fn build_client(client: Client, work: Arc<Work>, number: usize) -> TestResult<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for round in 0..ROUNDS {
        let id = format!("{}-build-{number}-{round}", work.prefix);
        run(
            &client,
            &id,
            "lean_build",
            &work.build(&id, number.saturating_add(round)),
        )
        .await?;
        let reply = settle(&client, &id).await?;
        outcomes.push(Outcome {
            id,
            kind: Kind::Build,
            reply,
        });
    }
    Ok(outcomes)
}

/// Call `tool` until the server answers, trying for up to [`RESTART`]: the
/// server may be down for a restart. Fails on a JSON-RPC error.
async fn call(client: &Client, tool: &str, arguments: &Value) -> TestResult<Value> {
    let until = Instant::now().checked_add(RESTART).ok_or("patience")?;
    loop {
        // A boxed error is not `Send`, so none lives across the sleep.
        let failed = match client.send(tool, arguments).await {
            Ok(response) => {
                return Ok(response
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("{tool} failed: {response}"))?);
            }
            Err(error) => error.to_string(),
        };
        if Instant::now() >= until {
            return Err(failed.into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// `lean_run` request `id`, trying until the server answers.
async fn run(client: &Client, id: &str, tool: &str, arguments: &Value) -> TestResult {
    let result = call(
        client,
        "lean_run",
        &json!({"request_id": id, "tool": tool, "arguments": arguments}),
    )
    .await?;
    if http::structured(&result)?.get("request").is_some() {
        Ok(())
    } else {
        Err(format!("lean_run {id} failed: {result}").into())
    }
}

/// Wait until request `id` ends, trying until the server answers; return
/// `lean_get`'s `{request, result}`.
async fn settle(client: &Client, id: &str) -> TestResult<Value> {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    loop {
        let result = call(client, "lean_get", &json!({"request_id": id, "wait_seconds": 30})).await?;
        let reply = http::structured(&result)?;
        if http::is_final(reply) {
            return Ok(reply.clone());
        }
        if Instant::now() >= until {
            return Err(format!("request {id} did not end: {reply}").into());
        }
    }
}

/// Replace the text of `path` in one step, so no check reads half a file.
fn replace(path: &Path, text: &str) -> TestResult {
    let next = path.with_extension("lean.next");
    std::fs::write(&next, text)?;
    std::fs::rename(&next, path)?;
    Ok(())
}

/// The lines of `marker.txt` in `root`: one per build that reached its
/// marker step.
fn marker_lines(root: &Path) -> TestResult<Vec<String>> {
    match std::fs::read_to_string(root.join("marker.txt")) {
        Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Wait until `marker.txt` in `root` holds `count` lines.
async fn wait_for_markers(root: &Path, count: usize) -> TestResult {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    while marker_lines(root)?.len() < count {
        if Instant::now() >= until {
            return Err(format!("fewer than {count} builds reached their marker step").into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Wait until the build tagged `tag` reaches its marker step.
async fn wait_for_marker(root: &Path, tag: &str) -> TestResult {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience")?;
    while !marker_lines(root)?.iter().any(|line| line == tag) {
        if Instant::now() >= until {
            return Err(format!("the build {tag} never reached its marker step").into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Poll the server's pool every 50 ms until `stop`; return the most workers
/// it held and its limit.
async fn watch_pool(client: Client, stop: Arc<AtomicBool>) -> TestResult<(u64, u64)> {
    let mut most = 0;
    let mut limit = 0;
    while !stop.load(Ordering::SeqCst) {
        let status = client.runtime().await?;
        let held = status.pointer("/pool/occupied_slots").and_then(Value::as_u64);
        most = most.max(held.ok_or_else(|| format!("no occupied_slots: {status}"))?);
        limit = status
            .pointer("/effective_settings/broker/max_workers")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("no max_workers: {status}"))?;
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    Ok((most, limit))
}

/// What the `ps` sampler saw.
#[derive(Clone, Copy, Debug, Default)]
struct Sampled {
    samples: u64,
    failures: u64,
    most_writers: usize,
    most_workers: usize,
}

/// A thread that lists the processes every 50 ms and counts the Lake builds
/// of the servers on a store and the workers of the server.
struct Sampler {
    stop: Arc<AtomicBool>,
    /// The server whose workers it counts; 0 for none.
    server: Arc<AtomicU32>,
    sampled: Arc<Mutex<Sampled>>,
    thread: std::thread::JoinHandle<()>,
}

impl Sampler {
    fn start(store: &Path, server: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let server = Arc::new(AtomicU32::new(server));
        let sampled = Arc::new(Mutex::new(Sampled::default()));
        let thread = std::thread::spawn({
            let (store, stop, server, sampled) = (
                store.to_path_buf(),
                Arc::clone(&stop),
                Arc::clone(&server),
                Arc::clone(&sampled),
            );
            move || {
                while !stop.load(Ordering::SeqCst) {
                    let seen = http::processes().ok().map(|all| {
                        let pid = server.load(Ordering::SeqCst);
                        let workers = if pid == 0 { 0 } else { http::workers(&all, pid).len() };
                        (http::lake_builds(&all, &store).len(), workers)
                    });
                    {
                        let mut sampled = sampled.lock();
                        match seen {
                            Some((writers, workers)) => {
                                sampled.samples = sampled.samples.saturating_add(1);
                                sampled.most_writers = sampled.most_writers.max(writers);
                                sampled.most_workers = sampled.most_workers.max(workers);
                            }
                            None => sampled.failures = sampled.failures.saturating_add(1),
                        }
                    }
                    std::thread::sleep(SAMPLE_EVERY);
                }
            }
        });
        Self {
            stop,
            server,
            sampled,
            thread,
        }
    }

    fn stop(self) -> TestResult<Sampled> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().map_err(|_| "the sampler failed")?;
        Ok(*self.sampled.lock())
    }
}

/// Say what the run did.
fn report(outcomes: &[Outcome], sampled: &Sampled) {
    let mut counts = std::collections::BTreeMap::<(&str, &str), usize>::new();
    for outcome in outcomes {
        let kind = match outcome.kind {
            Kind::File(..) => "checks",
            Kind::Trial => "trials",
            Kind::Build => "builds",
        };
        let count = counts
            .entry((kind, http::state(&outcome.reply).unwrap_or("?")))
            .or_default();
        *count = count.saturating_add(1);
    }
    for ((kind, state), count) in counts {
        eprintln!("{kind} {state}: {count}");
    }
    for outcome in outcomes {
        let error = outcome
            .reply
            .pointer("/result/structuredContent/error")
            .filter(|_| outcome.reply.pointer("/result/isError") == Some(&json!(true)));
        if let Some(error) = error {
            eprintln!("{} failed: {error}", outcome.id);
        }
    }
    eprintln!("ps sampler: {sampled:?}");
}

/// The assertions that hold on the fixture and on a running server. Each
/// trial's output must hold `printed`.
fn assert_common(outcomes: &[Outcome], sampled: &Sampled, max_workers: usize, printed: &str) -> TestResult {
    assert!(sampled.samples > 0, "the sampler saw nothing: {sampled:?}");
    assert!(
        sampled.most_writers <= 1,
        "more than one Lake writer at once: {sampled:?}"
    );
    assert!(
        sampled.most_workers <= max_workers,
        "more than {max_workers} workers: {sampled:?}"
    );
    let mut steps = Vec::new();
    for outcome in outcomes {
        assert!(http::is_final(&outcome.reply), "{} did not end", outcome.id);
        match &outcome.kind {
            Kind::File(file, digests) => check_read(outcome, file, digests)?,
            Kind::Trial => check_trial(outcome, printed)?,
            Kind::Build => steps.extend(build_steps(outcome)?),
        }
    }
    steps.sort_unstable();
    for pair in steps.windows(2) {
        if let [(_, earlier_end, earlier), (later_start, _, later)] = pair {
            assert!(
                earlier_end <= later_start,
                "Lake writer steps overlap: {earlier} and {later}"
            );
        }
    }
    Ok(())
}

/// Check what a check request saw: its digest names a text the file held,
/// and it reports no missing import.
fn check_read(outcome: &Outcome, file: &str, digests: &[String; 2]) -> TestResult {
    let id = &outcome.id;
    match http::state(&outcome.reply) {
        Some("interrupted") => Ok(()),
        Some("done") => {
            let result = outcome.reply.get("result").ok_or("no result")?;
            let text = result.to_string();
            assert!(
                !http::names_missing_import(&text),
                "check {id} of {file} reports a missing import: {text}"
            );
            assert_ne!(
                result.pointer("/structuredContent/trust/session_id"),
                Some(&json!("metadata-only")),
                "check {id} of {file} says Lean elaborated nothing: {text}"
            );
            let digest = result
                .pointer("/structuredContent/trust/source_sha256")
                .and_then(Value::as_str);
            assert!(
                digest.is_some_and(|digest| digests.iter().any(|known| known == digest)),
                "check {id} of {file} names {digest:?}, not the text it read ({digests:?}): {text}"
            );
            Ok(())
        }
        other => Err(format!("check {id} ended {other:?}").into()),
    }
}

/// Check what a trial saw: Lean ran its commands without an error, and its
/// output holds `printed`.
fn check_trial(outcome: &Outcome, printed: &str) -> TestResult {
    let id = &outcome.id;
    match http::state(&outcome.reply) {
        // The killed server ran it; it has no result.
        Some("interrupted") => Ok(()),
        Some("done") => {
            let result = outcome.reply.get("result").ok_or("no result")?;
            assert_ne!(result.get("isError"), Some(&json!(true)), "trial {id} failed: {result}");
            let data = result
                .pointer("/structuredContent/data")
                .ok_or_else(|| format!("trial {id} has no data: {result}"))?;
            assert_eq!(
                data.pointer("/diagnostics/summary/errors"),
                Some(&json!(0)),
                "trial {id} reports errors: {result}"
            );
            let output = data
                .pointer("/output/value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                output.contains(printed),
                "trial {id} did not print {printed:?}: {result}"
            );
            Ok(())
        }
        other => Err(format!("trial {id} ended {other:?}").into()),
    }
}

/// The Lake steps of a build request that ran: start, end and a label.
fn build_steps(outcome: &Outcome) -> TestResult<Vec<(u64, u64, String)>> {
    let id = &outcome.id;
    match http::state(&outcome.reply) {
        // The killed server ran it; it has no result.
        Some("interrupted") => Ok(Vec::new()),
        Some("done") => {
            let result = outcome.reply.get("result").ok_or("no result")?;
            assert_eq!(
                result.pointer("/structuredContent/ok"),
                Some(&json!(true)),
                "build {id} failed: {result}"
            );
            let steps = result
                .pointer("/structuredContent/steps")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("build {id} lists no steps: {result}"))?;
            steps
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    let at = |field: &str| {
                        step.get(field)
                            .and_then(Value::as_u64)
                            .ok_or_else(|| format!("build {id} step {index} has no {field}"))
                    };
                    Ok((
                        at("started_at_ms")?,
                        at("finished_at_ms")?,
                        format!("{id} step {index}"),
                    ))
                })
                .collect()
        }
        other => Err(format!("build {id} ended {other:?}").into()),
    }
}
