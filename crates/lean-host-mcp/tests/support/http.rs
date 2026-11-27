//! A server in a child process that a test can kill and restart, a stateless
//! MCP client for it, and readers of its request table and of the process
//! list.
//!
//! The server listens on a loopback port the test picks, so a restart on the
//! same store serves the same address.

use std::collections::BTreeSet;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::watch;

use super::TestResult;

/// The server binary under test.
pub(crate) const SERVER: &str = env!("CARGO_BIN_EXE_lean-host-mcp");

/// The protocol the client speaks: one HTTP request per call, no session.
const PROTOCOL: &str = "2026-07-28";

/// The writer helper setting a test server must not inherit.
const WRITER_ENV: &str = "LEAN_RS_WRITER_EXECUTABLE";

/// The row states after which a request never changes.
pub(crate) const FINAL_STATES: [&str; 3] = ["done", "timed_out", "interrupted"];

/// The directories a test server keeps, removed when the value drops.
pub(crate) struct Scratch {
    /// The server's store: request table, build logs and native state.
    pub(crate) store: PathBuf,
    /// A directory for sockets, from [`super::socket_dir`].
    pub(crate) sockets: PathBuf,
    _store: tempfile::TempDir,
    _sockets: tempfile::TempDir,
}

impl Scratch {
    /// New, empty directories under the target directory.
    pub(crate) fn new() -> TestResult<Self> {
        let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        let store = tempfile::Builder::new().prefix("store-").tempdir_in(tmp)?;
        let sockets = super::socket_dir()?;
        Ok(Self {
            store: store.path().canonicalize()?,
            sockets: sockets.path().canonicalize()?,
            _store: store,
            _sockets: sockets,
        })
    }
}

/// How to start a server.
#[derive(Clone, Debug)]
pub(crate) struct Launch {
    /// The Lake root it serves.
    pub(crate) root: PathBuf,
    /// Its store.
    pub(crate) store: PathBuf,
    /// Its socket directory.
    pub(crate) sockets: PathBuf,
    /// The loopback address it listens on.
    pub(crate) bind: SocketAddr,
    /// More environment, set last.
    pub(crate) envs: Vec<(String, String)>,
}

impl Launch {
    /// A launch for `root` with the directories of `scratch`, on a free port.
    pub(crate) fn new(root: &Path, scratch: &Scratch) -> TestResult<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            store: scratch.store.clone(),
            sockets: scratch.sockets.clone(),
            bind: free_port()?,
            envs: Vec::new(),
        })
    }

    /// This launch with `name` set to `value` in the server's environment.
    #[must_use]
    pub(crate) fn env(mut self, name: &str, value: &str) -> Self {
        self.envs.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The MCP endpoint of a server started from this launch.
    pub(crate) fn url(&self) -> String {
        format!("http://{}/mcp", self.bind)
    }
}

/// A loopback address no socket holds now.
fn free_port() -> TestResult<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

/// A running server. Dropping it kills the process.
pub(crate) struct Server {
    child: Child,
    /// Its MCP endpoint.
    pub(crate) url: String,
    log: Arc<Mutex<String>>,
}

impl Server {
    /// Start the server of `launch` and wait up to `patience` until it
    /// listens. Recovery runs before that, so a restart may take a while.
    pub(crate) async fn start(launch: &Launch, patience: Duration) -> TestResult<Self> {
        let mut command = Command::new(SERVER);
        command
            .kill_on_drop(true)
            .current_dir(&launch.root)
            .arg("--bind")
            .arg(launch.bind.to_string())
            .arg("--lake-root")
            .arg(&launch.root)
            .env("LEAN_HOST_MCP_STORE_DIR", &launch.store)
            .env("LEAN_HOST_MCP_SOCKET_DIR", &launch.sockets)
            .env("LEAN_HOST_MCP_RESPONSE_CARRIER", "both")
            .env("RUST_LOG", "warn,lean_host_mcp=info")
            .env("NO_COLOR", "1")
            .env_remove("LEAN_HOST_MCP_PROJECT")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command.env_remove(WRITER_ENV);
        // `cargo test` adds its library path (`DYLD_FALLBACK_LIBRARY_PATH` on
        // macOS, `LD_LIBRARY_PATH` on Linux). A server started by hand has
        // none, and workers refuse to start with one.
        for (name, _) in std::env::vars_os() {
            let key = name.as_encoded_bytes();
            if key.starts_with(b"DYLD_") || key.starts_with(b"LD_") || key.starts_with(b"__XPC_DYLD_") {
                command.env_remove(&name);
            }
        }
        // The server inherits the caller's `LEAN_HOST_MCP_WORKERS_DIR`. The
        // suites that start workers require it: see
        // `super::installed_workers_dir`.
        for (name, value) in &launch.envs {
            command.env(name, value);
        }
        let mut child = command.spawn()?;
        let stderr = child.stderr.take().ok_or("the server has no stderr")?;
        let log = Arc::new(Mutex::new(String::new()));
        let (listening, mut heard) = watch::channel(false);
        tokio::spawn(keep_log(stderr, Arc::clone(&log), listening));
        let ready = tokio::time::timeout(patience, heard.wait_for(|listening| *listening))
            .await
            .map(|heard| heard.is_ok());
        match ready {
            Ok(true) => Ok(Self {
                child,
                url: launch.url(),
                log,
            }),
            Ok(false) => Err(format!("the server exited before it listened:\n{}", log.lock()).into()),
            Err(_) => Err(format!("the server did not listen within {patience:?}:\n{}", log.lock()).into()),
        }
    }

    /// The server's log so far.
    pub(crate) fn log(&self) -> String {
        self.log.lock().clone()
    }

    /// The server's process id.
    pub(crate) fn pid(&self) -> TestResult<u32> {
        Ok(self.child.id().ok_or("the server has exited")?)
    }

    /// Kill the server with SIGKILL, wait until it is gone, and return its log.
    pub(crate) async fn kill(mut self) -> TestResult<String> {
        self.child.start_kill()?;
        tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await??;
        Ok(self.log())
    }

    /// Stop the server with SIGTERM, wait until it exits, and return its log.
    /// Fails unless it exits with status 0.
    pub(crate) async fn stop(mut self) -> TestResult<String> {
        signal("-TERM", self.pid()?)?;
        let status = tokio::time::timeout(Duration::from_mins(2), self.child.wait()).await??;
        if status.success() {
            Ok(self.log())
        } else {
            Err(format!("the server exited with {status}:\n{}", self.log()).into())
        }
    }
}

/// Send `signal`, such as `-TERM`, to process `pid`.
pub(crate) fn signal(signal: &str, pid: u32) -> TestResult {
    let status = std::process::Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill {signal} {pid}: {status}").into())
    }
}

/// Copy the server's stderr into `log`, and say when the server listens.
async fn keep_log(stderr: ChildStderr, log: Arc<Mutex<String>>, listening: watch::Sender<bool>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let heard = line.contains("Streamable HTTP transport listening");
        {
            let mut text = log.lock();
            text.push_str(&line);
            text.push('\n');
        }
        if heard {
            listening.send_replace(true);
        }
    }
}

/// A client that sends each call as one HTTP request, with no session.
#[derive(Clone, Debug)]
pub(crate) struct Client {
    http: reqwest::Client,
    url: String,
    name: String,
    next: Arc<AtomicU64>,
}

impl Client {
    /// A client for the endpoint `url` that calls itself `name`.
    pub(crate) fn new(url: &str, name: &str) -> TestResult<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
            url: url.to_owned(),
            name: name.to_owned(),
            next: Arc::new(AtomicU64::new(1)),
        })
    }

    /// The JSON-RPC request that calls `tool` with `arguments`.
    fn body(&self, id: u64, tool: &str, arguments: &Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": PROTOCOL,
                    "io.modelcontextprotocol/clientInfo": {"name": self.name, "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string()
    }

    /// Call `tool` and return the JSON-RPC response, which holds `result` or
    /// `error`.
    pub(crate) async fn send(&self, tool: &str, arguments: &Value) -> TestResult<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let response = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", tool)
            .body(self.body(id, tool, arguments))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(format!("{tool}: HTTP {status}: {text}").into());
        }
        let mut response = message(&text, id)?;
        if let Some(result) = response.get_mut("result") {
            fill_structured(result);
        }
        Ok(response)
    }

    /// Call `tool` and return its result. Fails on a JSON-RPC error.
    pub(crate) async fn call(&self, tool: &str, arguments: &Value) -> TestResult<Value> {
        let response = self.send(tool, arguments).await?;
        Ok(response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("{tool} failed: {response}"))?)
    }

    /// `lean_run`: the tool result.
    pub(crate) async fn run_result(&self, id: &str, tool: &str, arguments: &Value) -> TestResult<Value> {
        self.call(
            "lean_run",
            &json!({"request_id": id, "tool": tool, "arguments": arguments}),
        )
        .await
    }

    /// `lean_run`: start request `id` and return its row.
    pub(crate) async fn run(&self, id: &str, tool: &str, arguments: &Value) -> TestResult<Value> {
        let result = self.run_result(id, tool, arguments).await?;
        Ok(structured(&result)?
            .get("request")
            .cloned()
            .ok_or_else(|| format!("lean_run {id} returned no row: {result}"))?)
    }

    /// `lean_get` with a wait of up to `wait_seconds`: `{request, result}`.
    pub(crate) async fn get(&self, id: &str, wait_seconds: u64) -> TestResult<Value> {
        let result = self
            .call("lean_get", &json!({"request_id": id, "wait_seconds": wait_seconds}))
            .await?;
        Ok(structured(&result)?.clone())
    }

    /// Wait up to `patience` until request `id` ends, and return `lean_get`'s
    /// `{request, result}`.
    pub(crate) async fn ended(&self, id: &str, patience: Duration) -> TestResult<Value> {
        let until = Instant::now().checked_add(patience).ok_or("patience is too long")?;
        loop {
            let reply = self.get(id, 30).await?;
            if is_final(&reply) {
                return Ok(reply);
            }
            if Instant::now() >= until {
                return Err(format!("request {id} did not end within {patience:?}: {reply}").into());
            }
        }
    }

    /// `lean_runtime`'s structured reply: build, settings and worker activity.
    pub(crate) async fn runtime(&self) -> TestResult<Value> {
        let result = self.call("lean_runtime", &json!({})).await?;
        Ok(structured(&result)?.clone())
    }

    /// A `curl` process that calls `tool` with `arguments` and waits for the
    /// reply: a client the test can kill.
    pub(crate) fn process(&self, tool: &str, arguments: &Value) -> TestResult<Child> {
        let mut command = Command::new("curl");
        command
            .kill_on_drop(true)
            .args(["--silent", "--show-error", "--no-buffer", "--request", "POST"])
            .args(["--header", "Accept: application/json, text/event-stream"])
            .args(["--header", "Content-Type: application/json"])
            .args(["--header", &format!("MCP-Protocol-Version: {PROTOCOL}")])
            .args(["--header", "Mcp-Method: tools/call"])
            .args(["--header", &format!("Mcp-Name: {tool}")])
            .arg("--data-binary")
            .arg(self.body(0, tool, arguments))
            .arg(&self.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command.spawn()?)
    }
}

/// The JSON-RPC message with id `id` in a reply body of JSON or of
/// server-sent events.
fn message(body: &str, id: u64) -> TestResult<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Ok(value);
    }
    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok(value) = serde_json::from_str::<Value>(&data)
            && value.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Ok(value);
        }
    }
    Err(format!("no reply with id {id} in: {body}").into())
}

/// Give a tool result that holds its reply only as JSON text, as a server on
/// the default `text` carrier sends it, the `structuredContent` that the
/// `both` carrier sends, so tests read one shape from any server. The stored
/// tool result in a `lean_get` reply's `result` gets the same.
fn fill_structured(result: &mut Value) {
    if let Some(map) = result.as_object_mut()
        && !map.contains_key("structuredContent")
        && let Some(reply) = map
            .get("content")
            .and_then(|content| content.pointer("/0/text"))
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .filter(Value::is_object)
    {
        map.insert("structuredContent".to_owned(), reply);
    }
    if let Some(stored) = result.pointer_mut("/structuredContent/result") {
        fill_structured(stored);
    }
}

/// The structured content of a tool result.
pub(crate) fn structured(result: &Value) -> TestResult<&Value> {
    Ok(result
        .get("structuredContent")
        .ok_or_else(|| format!("no structured content: {result}"))?)
}

/// The state of the row in a `lean_get` reply.
pub(crate) fn state(reply: &Value) -> Option<&str> {
    reply.pointer("/request/state").and_then(Value::as_str)
}

/// Whether the row in a `lean_get` reply has ended.
pub(crate) fn is_final(reply: &Value) -> bool {
    state(reply).is_some_and(|state| FINAL_STATES.contains(&state))
}

/// The code of a request API error in a tool result.
pub(crate) fn error_code(result: &Value) -> Option<&str> {
    result.pointer("/structuredContent/error/code").and_then(Value::as_str)
}

/// The request id a tool result or a JSON-RPC error names.
pub(crate) fn reply_id(response: &Value) -> Option<&str> {
    response
        .pointer("/result/structuredContent/request_id")
        .or_else(|| response.pointer("/error/data/request_id"))
        .and_then(Value::as_str)
}

/// The hex SHA-256 of `bytes`, as replies name a checked file's text.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// One row of a request table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TableRow {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) state: String,
}

/// The rows of the request table in `store`, oldest first.
pub(crate) fn table(store: &Path) -> TestResult<Vec<TableRow>> {
    use rusqlite::OpenFlags;
    let path = store.join(lean_host_mcp::requests::store::DATABASE_NAME);
    let db = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(10))?;
    let mut select = db.prepare("SELECT id, kind, state FROM requests ORDER BY created_at, id")?;
    let rows = select
        .query_map([], |row| {
            Ok(TableRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                state: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One process, as `ps` lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Process {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) command: String,
}

/// Every process on the machine.
pub(crate) fn processes() -> TestResult<Vec<Process>> {
    let output = std::process::Command::new("ps")
        .args(["-axwwo", "pid=,ppid=,command="])
        .output()?;
    if !output.status.success() {
        return Err(format!("ps failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let ppid = words.next()?.parse().ok()?;
            let command = words.collect::<Vec<_>>().join(" ");
            Some(Process { pid, ppid, command })
        })
        .collect())
}

/// The processes under `pid`, found by parent.
pub(crate) fn descendants(pid: u32) -> TestResult<Vec<Process>> {
    Ok(descendants_in(&processes()?, pid))
}

/// The processes of `all` under `pid`, found by parent.
pub(crate) fn descendants_in(all: &[Process], pid: u32) -> Vec<Process> {
    let mut parents = vec![pid];
    let mut found = Vec::new();
    while let Some(parent) = parents.pop() {
        for process in all.iter().filter(|process| process.ppid == parent) {
            parents.push(process.pid);
            found.push(process.clone());
        }
    }
    found
}

/// The processes of `all` under server `server` that run the worker binary,
/// at any depth.
pub(crate) fn worker_processes(all: &[Process], server: u32) -> Vec<Process> {
    descendants_in(all, server)
        .into_iter()
        .filter(|process| {
            process
                .command
                .split_whitespace()
                .next()
                .is_some_and(|program| program.ends_with("lean-host-mcp-worker"))
        })
        .collect()
}

/// Watch `processes` every 50 ms, for up to `limit` after `since`, and say
/// how long each lived.
pub(crate) fn watch_until_gone(processes: &[Process], since: Instant, limit: Duration) -> Vec<String> {
    let mut left: Vec<&Process> = processes.iter().collect();
    let mut lines = Vec::new();
    while !left.is_empty() && since.elapsed() < limit {
        let live: BTreeSet<u32> = match self::processes() {
            Ok(all) => all.iter().map(|process| process.pid).collect(),
            Err(error) => {
                lines.push(format!("ps failed: {error}"));
                break;
            }
        };
        left.retain(|process| {
            let alive = live.contains(&process.pid);
            if !alive {
                lines.push(format!(
                    "pid {} gone after {:?}: {}",
                    process.pid,
                    since.elapsed(),
                    process.command
                ));
            }
            alive
        });
        std::thread::sleep(Duration::from_millis(50));
    }
    for process in left {
        lines.push(format!(
            "pid {} still running after {:?}: {}",
            process.pid,
            since.elapsed(),
            process.command
        ));
    }
    lines
}

/// The Lake processes that writer helpers for `root` run. A writer helper
/// runs `<server> writer <root> ...` and Lake runs as its child.
pub(crate) fn lake_writers<'a>(processes: &'a [Process], root: &Path) -> Vec<&'a Process> {
    let root = root.to_string_lossy();
    let helpers: BTreeSet<u32> = processes
        .iter()
        .filter(|process| {
            let mut words = process.command.split_whitespace().skip(1);
            words.next() == Some("writer") && process.command.contains(root.as_ref())
        })
        .map(|process| process.pid)
        .collect();
    processes
        .iter()
        .filter(|process| helpers.contains(&process.ppid) && is_lake(&process.command))
        .collect()
}

/// Whether `command` runs Lake.
fn is_lake(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|program| program == "lake" || program.ends_with("/lake"))
}

/// The processes server `parent` runs Lean in for its calls: workers, and the
/// native readers (`<server> native-reader-exec`) that run file checks. Both
/// count against `broker.max_workers`.
pub(crate) fn workers(processes: &[Process], parent: u32) -> Vec<&Process> {
    processes
        .iter()
        .filter(|process| {
            let mut words = process.command.split_whitespace();
            let worker = words
                .next()
                .is_some_and(|program| program.ends_with("lean-host-mcp-worker"));
            process.ppid == parent && (worker || words.next() == Some("native-reader-exec"))
        })
        .collect()
}

/// Whether a check's reply says that an import is missing or not built.
pub(crate) fn names_missing_import(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "needs_build",
        "missing_imports",
        "unknown module",
        "unknown import",
        "module not found",
    ]
    .iter()
    .any(|sign| text.contains(sign))
        || (text.contains(".olean")
            && ["does not exist", "no such file", "object file"]
                .iter()
                .any(|sign| text.contains(sign)))
}
