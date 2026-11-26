//! Workers that load a Lake project's own build: a trial that imports a module
//! of a path dependency and prints its axioms, a proof position and a
//! declaration lookup, a check of a module that has no source file, the
//! refusal of an import whose roots differ from the worker's launch roots or
//! hold `Init`, a worker whose launch roots leave the project, and builds that
//! stop the project's workers before Lake starts.
//!
//! The tests are ignored: each builds a chain project (`support::chain`) and
//! needs an installed worker. Set `LEAN_HOST_MCP_TEST_FIXTURE` or
//! `LEAN_SYSROOT`, and `LEAN_HOST_MCP_WORKERS_DIR` to a directory that
//! `lean-host-mcp install-worker --workers-dir` wrote.
//!
//! The tests run one at a time. The SDK's writer helper refuses to start
//! while any process in the shared owner registry is starting, so a build in
//! one test would fail whenever another test starts a worker at that moment.

#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use serial_test::serial;
use support::TestResult;
use support::chain::{Chain, chain_project};
use support::http::{self, Client, Launch, Scratch, Server};
use tokio::task::JoinHandle;

/// How long a test waits for a server to listen.
const STARTUP: Duration = Duration::from_mins(2);
/// How long a test waits for a request to end or a file to appear.
const PATIENCE: Duration = Duration::from_mins(5);
const SAMPLE_EVERY: Duration = Duration::from_millis(50);
/// The new text of `Chain/A.lean`, in which `a` has another type.
const EDITED_A: &str = "def a : Int := 1\n";
/// A recipe whose first step holds the write turn for two seconds before Lake
/// builds `Chain.A`.
const SLOW_BUILD: &str = r#"
[recipes.slow]
steps = [["env", "sh", "-c", "touch building.txt; sleep 2"], ["build", "Chain.A"]]
"#;

/// The chain's lakefile with a second path dependency, `dep2`.
const TWO_DEPENDENCIES: &str = "import Lake\nopen Lake DSL\n\npackage chain\n\nrequire dep from \"dep\"\nrequire dep2 from \
                                \"dep2\"\n\n@[default_target]\nlean_lib Chain\n";

/// A built chain project. Fails unless an installed worker is named.
fn project() -> TestResult<Chain> {
    std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    chain_project()
}

/// The `data` of semantic tool result `result`. Fails when the result is an
/// error or names an issue that is not a warning.
fn data(result: &Value) -> TestResult<&Value> {
    let structured = http::structured(result)?;
    let failed = result.get("isError") == Some(&json!(true))
        || structured
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|issues| {
                issues
                    .iter()
                    .any(|issue| issue.get("severity").and_then(Value::as_str) != Some("warning"))
            });
    if failed {
        return Err(format!("the call failed: {result}").into());
    }
    Ok(structured
        .get("data")
        .filter(|data| !data.is_null())
        .ok_or_else(|| format!("no data: {result}"))?)
}

/// The `lean_lookup` arguments for declaration `name` with `imports`.
fn lookup(name: &str, imports: &[&str]) -> Value {
    json!({"kind": "declaration", "name": name, "imports": imports})
}

/// The statement `lean_lookup` gives for declaration `name` with `imports`.
async fn statement(client: &Client, name: &str, imports: &[&str]) -> TestResult<String> {
    let result = client.call("lean_lookup", &lookup(name, imports)).await?;
    let data = data(&result)?;
    assert_eq!(data.get("status"), Some(&json!("found")), "{result}");
    Ok(data
        .pointer("/declaration/statement/value")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("no statement: {result}"))?
        .to_owned())
}

/// The `lean_trial` arguments that run `commands` with `imports`.
fn trial(imports: &[&str], commands: &str) -> Value {
    json!({"kind": "command", "imports": imports, "commands": commands})
}

/// Lean commands that write `started`, sleep for `millis` ms, then write
/// `finished`.
fn sleeper(started: &Path, finished: &Path, millis: u32) -> TestResult<String> {
    let quote = |path: &Path| -> TestResult<String> {
        Ok(serde_json::to_string(path.to_str().ok_or("a path that is not UTF-8")?)?)
    };
    Ok(format!(
        "#eval do\n  IO.FS.writeFile {} \"started\"\n  IO.sleep {millis}\n  IO.FS.writeFile {} \"finished\"\n",
        quote(started)?,
        quote(finished)?
    ))
}

/// Call `tool` in a task of its own, which returns the JSON-RPC response:
/// a `result` or an `error`. A boxed error is not `Send`, so the task returns
/// its message.
fn spawn_send(client: &Client, tool: &'static str, arguments: Value) -> JoinHandle<Result<Value, String>> {
    let client = client.clone();
    tokio::spawn(async move { client.send(tool, &arguments).await.map_err(|error| error.to_string()) })
}

/// The tool result in JSON-RPC response `response`.
fn result(response: &Value) -> TestResult<&Value> {
    Ok(response.get("result").ok_or_else(|| format!("no result: {response}"))?)
}

/// Wait until `path` exists.
async fn wait_for(path: &Path) -> TestResult {
    let until = Instant::now().checked_add(PATIENCE).ok_or("patience is too long")?;
    while !path.exists() {
        if Instant::now() >= until {
            return Err(format!("{} did not appear within {PATIENCE:?}", path.display()).into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Send trial `second` while a sleeping trial holds the chain's one worker, and
/// write `changed` to the chain's manifest while `second` waits in that
/// worker's queue. The worker reads the manifest at `second`'s import. Returns
/// the response to `second` once the manifest is back as it was.
async fn behind_a_manifest_change(chain: &Chain, client: &Client, changed: &Value, second: Value) -> TestResult<Value> {
    let manifest = chain.root.join("lake-manifest.json");
    let original = std::fs::read_to_string(&manifest)?;
    let started = chain.root.join("started.txt");
    let finished = chain.root.join("finished.txt");
    let first = spawn_send(
        client,
        "lean_trial",
        trial(&["Chain.A"], &sleeper(&started, &finished, 6000)?),
    );
    wait_for(&started).await?;
    let second = spawn_send(client, "lean_trial", second);
    // Give the second trial time to join the busy worker's queue.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(&manifest, changed.to_string())?;
    let first = first.await??;
    let second = second.await??;
    std::fs::write(&manifest, &original)?;
    data(result(&first)?)?;
    assert!(finished.exists(), "the first trial did not finish: {first}");
    Ok(second)
}

/// The chain's manifest, with `change` applied to its package list.
fn manifest_with(chain: &Chain, change: impl FnOnce(&mut Vec<Value>)) -> TestResult<Value> {
    let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(chain.root.join("lake-manifest.json"))?)?;
    change(
        manifest
            .get_mut("packages")
            .and_then(Value::as_array_mut)
            .ok_or("the manifest lists no packages")?,
    );
    Ok(manifest)
}

/// The ids of the worker processes under server `server` now.
fn worker_ids(server: u32) -> TestResult<Vec<u32>> {
    Ok(http::worker_processes(&http::processes()?, server)
        .iter()
        .map(|process| process.pid)
        .collect())
}

/// Milliseconds since the Unix epoch, as build steps record their times.
fn now_ms() -> TestResult<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

/// A command trial imports `Dep`, a module of the path dependency `dep`, from
/// that package's build, and `#print axioms` lists the axioms of a theorem in
/// it, in the form the stack gate's axiom check reads.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_trial_imports_a_dependency_module_and_prints_its_axioms() -> TestResult {
    let chain = project()?;
    chain.write(
        "dep/Dep.lean",
        "def dep : Nat := 1\n\ntheorem dep_em (p : Prop) : p ∨ ¬p := Classical.em p\n",
    )?;
    chain.lake(&["build"])?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "trial")?;
    let result = client
        .call("lean_trial", &trial(&["Dep"], "#check dep_em\n#print axioms dep_em"))
        .await?;
    let data = data(&result)?;
    assert_eq!(data.pointer("/diagnostics/summary/errors"), Some(&json!(0)), "{result}");
    let output = data
        .pointer("/output/value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(output.contains("dep_em (p : Prop) : p ∨ ¬p"), "{result}");
    assert!(
        output.contains("'dep_em' depends on axioms: [") && output.contains("Classical.choice"),
        "{result}"
    );
    server.stop().await?;
    Ok(())
}

/// A proof position in a file that imports the project's modules returns its
/// goal, and a declaration lookup returns the type of a project declaration.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_proof_position_and_a_lookup_answer_from_the_project_build() -> TestResult {
    let chain = project()?;
    chain.write(
        "Chain/D.lean",
        "import Chain.B\n\ntheorem b_le : b ≤ b := by\n  exact Nat.le_refl b\n",
    )?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "context")?;
    let result = client
        .call(
            "lean_context",
            &json!({"kind": "proof_position", "file": "Chain/D.lean", "declaration": "b_le"}),
        )
        .await?;
    let context = data(&result)?;
    assert_eq!(context.get("status"), Some(&json!("context")), "{result}");
    let goals = context
        .get("goals_before")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("no goals: {result}"))?;
    assert!(
        goals
            .iter()
            .filter_map(Value::as_str)
            .any(|goal| goal.contains("⊢ b ≤ b")),
        "{result}"
    );
    let statement = statement(&client, "b", &["Chain.B"]).await?;
    assert!(statement.contains("Nat"), "{statement}");
    server.stop().await?;
    Ok(())
}

/// `lean_verify` sends a module that has no source file to a worker, which
/// imports the module from the project's build: its `.ilean` index names the
/// declarations. The SDK's declaration check finds only declarations
/// elaborated from source, so the row reads `not_found`, not `needs_build`.
/// The command trial that the stack gate then sends prints the axioms.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_module_without_a_source_file_is_checked_in_a_worker() -> TestResult {
    let chain = project()?;
    std::fs::remove_file(chain.root.join("Chain/C.lean"))?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "verify")?;
    let result = client
        .call(
            "lean_verify",
            &json!({"targets": [{"kind": "module_all", "module": "Chain.C"}], "report_axioms": true}),
        )
        .await?;
    let verified = data(&result)?;
    let rows = verified
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("no rows: {result}"))?;
    assert!(
        rows.iter().any(|row| {
            row.get("declaration") == Some(&json!("b_eq"))
                && row.get("verification_status") == Some(&json!("not_found"))
        }),
        "{result}"
    );
    assert_eq!(verified.pointer("/summary/needs_build"), Some(&json!(0)), "{result}");
    assert!(
        !worker_ids(server.pid()?)?.is_empty(),
        "no worker checked the module: {result}"
    );
    let result = client
        .call("lean_trial", &trial(&["Chain.C"], "#print axioms b_eq"))
        .await?;
    let output = data(&result)?
        .pointer("/output/value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(output.contains("'b_eq' does not depend on any axioms"), "{result}");
    server.stop().await?;
    Ok(())
}

/// A worker admits an import only through the roots it was launched with, in
/// their order. The project gains a second path dependency, `dep2`. A trial
/// that waits behind another on the project's one worker runs after the
/// manifest has swapped `dep` and `dep2`: the same packages, so the same
/// artifact domains, but Lean would search their roots in another order. The
/// worker refuses that import. With the manifest restored, the import answers.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn an_import_through_other_roots_is_refused() -> TestResult {
    let chain = project()?;
    chain.write("lakefile.lean", TWO_DEPENDENCIES)?;
    chain.write(
        "dep2/lakefile.toml",
        "name = \"dep2\"\n\n[[lean_lib]]\nname = \"Dep2\"\n",
    )?;
    chain.write("dep2/Dep2.lean", "def dep2 : Nat := 2\n")?;
    chain.write(
        "dep2/lake-manifest.json",
        "{\"version\": \"1.3.0\", \"packagesDir\": \".lake/packages\", \"packages\": [], \"name\": \"dep2\", \
         \"lakeDir\": \".lake\"}\n",
    )?;
    chain.lake(&["update"])?;
    chain.lake(&["build", "+Dep2"])?;
    let mut packages = 0;
    let swapped = manifest_with(&chain, |list| {
        packages = list.len();
        list.reverse();
    })?;
    assert_eq!(packages, 2, "{swapped}");
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "roots")?;
    let second = behind_a_manifest_change(&chain, &client, &swapped, trial(&["Dep"], "#check dep")).await?;
    assert!(
        second
            .to_string()
            .contains("import roots differ from this worker's launch roots"),
        "{second}\n{}",
        server.log()
    );
    let statement = statement(&client, "dep", &["Dep"]).await?;
    assert!(statement.contains("Nat"), "{statement}");
    server.stop().await?;
    Ok(())
}

/// A worker whose project drops a dependency while a check waits on it
/// restarts for the project's new artifact domains. Its launch roots still
/// name the dropped package's build, which no domain covers, so the restarted
/// worker refuses the check at once rather than exit before it connects. That
/// leaves nothing that blocks a writer: with the manifest restored, the next
/// check answers and a build runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_worker_whose_launch_roots_leave_the_project_refuses_at_once() -> TestResult {
    let chain = project()?;
    let dropped = manifest_with(&chain, Vec::clear)?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "dropped")?;
    let second = behind_a_manifest_change(&chain, &client, &dropped, trial(&["Dep"], "#check dep")).await?;
    assert!(
        second
            .to_string()
            .contains("launch root outside the project's artifact domains"),
        "{second}\n{}",
        server.log()
    );
    let statement = statement(&client, "dep", &["Dep"]).await?;
    assert!(statement.contains("Nat"), "{statement}");
    let build = tokio::time::timeout(
        Duration::from_mins(1),
        client.call("lean_build", &json!({"targets": ["Chain.A"]})),
    )
    .await??;
    assert_eq!(http::structured(&build)?.get("ok"), Some(&json!(true)), "{build}");
    server.stop().await?;
    Ok(())
}

/// A worker refuses an import while one of its roots holds `Init.olean`, which
/// Lean would load in place of the release's.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn an_import_root_that_holds_init_is_refused() -> TestResult {
    let chain = project()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "init")?;
    let statement = statement(&client, "a", &["Chain.A"]).await?;
    assert!(statement.contains("Nat"), "{statement}");
    let shadow = chain.root.join(".lake/build/lib/lean/Init.olean");
    std::fs::write(&shadow, b"")?;
    let refused = client.send("lean_lookup", &lookup("b", &["Chain.B"])).await;
    std::fs::remove_file(&shadow)?;
    let refused = refused?;
    assert!(
        refused.to_string().contains("shadows compiled package Init"),
        "{refused}"
    );
    server.stop().await?;
    Ok(())
}

/// After an edit, a build of the edited module stops the project's workers and
/// waits until each has exited before Lake starts: a `ps` sampler every 50 ms
/// sees no worker while Lake runs. The next lookup starts a fresh worker and
/// sees the new type.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_build_stops_the_project_workers_before_lake_starts() -> TestResult {
    let chain = project()?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let pid = server.pid()?;
    let client = Client::new(&server.url, "build")?;
    let before = statement(&client, "a", &["Chain.A"]).await?;
    assert!(before.contains("Nat"), "{before}");
    let old = worker_ids(pid)?;
    assert!(!old.is_empty(), "no worker after the first lookup");

    chain.write("Chain/A.lean", EDITED_A)?;
    let sampler = Sampler::start(&chain.root, pid)?;
    let build = client.call("lean_build", &json!({"targets": ["Chain.A"]})).await;
    let seen = sampler.stop()?;
    let build = build?;
    eprintln!("ps sampler: {seen:?}");
    assert_eq!(http::structured(&build)?.get("ok"), Some(&json!(true)), "{build}");
    assert!(seen.lake > 0, "the sampler never saw Lake: {seen:?}");
    assert!(seen.workers > 0, "the sampler never saw a worker: {seen:?}");
    assert!(seen.both.is_empty(), "a worker ran while Lake ran: {seen:?}");
    let live: Vec<u32> = http::processes()?.iter().map(|process| process.pid).collect();
    assert!(
        old.iter().all(|pid| !live.contains(pid)),
        "a worker from before the build still runs: {old:?}"
    );

    let after = statement(&client, "a", &["Chain.A"]).await?;
    assert!(after.contains("Int"), "{after}");
    let new = worker_ids(pid)?;
    assert!(
        !new.is_empty() && new.iter().all(|pid| !old.contains(pid)),
        "the lookup after the build did not start a fresh worker: {old:?} then {new:?}"
    );
    server.stop().await?;
    Ok(())
}

/// A lookup sent while a build holds the project waits until the build ends,
/// then answers from what the build wrote.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Lean toolchain and an installed worker; see the module docs"]
#[serial]
async fn a_check_sent_during_a_build_waits_and_answers_from_the_new_build() -> TestResult {
    let chain = project()?;
    chain.write("lean-host-mcp.toml", SLOW_BUILD)?;
    let scratch = Scratch::new()?;
    let server = Server::start(&Launch::new(&chain.root, &scratch)?, STARTUP).await?;
    let client = Client::new(&server.url, "during")?;
    let before = statement(&client, "a", &["Chain.A"]).await?;
    assert!(before.contains("Nat"), "{before}");

    chain.write("Chain/A.lean", EDITED_A)?;
    let id = "slow-build-0001";
    client.run(id, "lean_build", &json!({"recipe": "slow"})).await?;
    wait_for(&chain.root.join("building.txt")).await?;
    let after = statement(&client, "a", &["Chain.A"]).await?;
    let answered = now_ms()?;
    let build = client.ended(id, PATIENCE).await?;
    let outcome = build.get("result").ok_or_else(|| format!("no build result: {build}"))?;
    assert_eq!(outcome.pointer("/structuredContent/ok"), Some(&json!(true)), "{build}");
    let finished = outcome
        .pointer("/structuredContent/steps")
        .and_then(Value::as_array)
        .and_then(|steps| steps.last())
        .and_then(|step| step.get("finished_at_ms"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("no step end: {build}"))?;
    assert!(
        answered >= finished,
        "the lookup answered at {answered} ms, before the build ended at {finished} ms"
    );
    assert!(after.contains("Int"), "{after}");
    server.stop().await?;
    Ok(())
}

/// What the `ps` sampler saw.
#[derive(Clone, Debug, Default)]
struct Seen {
    samples: u64,
    /// Samples with a Lake writer for the root.
    lake: u64,
    /// Samples with a worker under the server.
    workers: u64,
    /// The samples with both: what each held.
    both: Vec<String>,
}

/// A thread that lists the processes every 50 ms and notes Lake writers for
/// the root and workers under the server.
struct Sampler {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Seen>,
}

impl Sampler {
    /// Start sampling, and return after the first sample.
    fn start(root: &Path, server: u32) -> TestResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (first, sampled) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn({
            let (root, stop): (PathBuf, _) = (root.to_path_buf(), Arc::clone(&stop));
            move || {
                let mut seen = Seen::default();
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(all) = http::processes() {
                        let lake = http::lake_writers(&all, &root);
                        let workers = http::worker_processes(&all, server);
                        seen.samples = seen.samples.saturating_add(1);
                        if !lake.is_empty() {
                            seen.lake = seen.lake.saturating_add(1);
                        }
                        if !workers.is_empty() {
                            seen.workers = seen.workers.saturating_add(1);
                        }
                        if !lake.is_empty() && !workers.is_empty() {
                            seen.both.push(format!("{lake:?} with {workers:?}"));
                        }
                        // The test waits for the first sample only, and a test
                        // that stopped waiting has dropped the receiver.
                        if seen.samples == 1 {
                            first.try_send(()).ok();
                        }
                    }
                    std::thread::sleep(SAMPLE_EVERY);
                }
                seen
            }
        });
        sampled.recv_timeout(PATIENCE)?;
        Ok(Self { stop, thread })
    }

    fn stop(self) -> TestResult<Seen> {
        self.stop.store(true, Ordering::SeqCst);
        Ok(self.thread.join().map_err(|_| "the sampler failed")?)
    }
}
