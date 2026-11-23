//! Stdio lifecycle regression tests.
//!
//! These tests speak newline-delimited MCP JSON-RPC to the compiled binary,
//! then close the exact stdin pipe owned by that child process. Cleanup uses
//! only the `Child` handle or PIDs discovered as direct children of that
//! handle; no process-name or command-substring killing is allowed here.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::test]
async fn stdio_shutdown_on_stdin_eof_exits_parent() {
    let registry = tempfile::tempdir().expect("temp registry");
    let server = StdioServer::start(&fixture_root(), registry.path());
    let pid = server.pid();

    let doctor = wait_for_doctor_pid(registry.path(), pid).await;
    assert!(
        doctor.contains(&format!("pid={pid}")),
        "doctor output should include exact server PID:\n{doctor}"
    );
    assert!(
        doctor.contains("parent_pid_at_start="),
        "doctor should report recorded parent PID:\n{doctor}"
    );
    assert!(
        doctor.contains("current_parent_pid="),
        "doctor should report current parent PID:\n{doctor}"
    );
    assert!(
        doctor.contains("process_group_id="),
        "doctor should report process group:\n{doctor}"
    );

    server.close_stdin_and_wait().await;
    assert!(
        wait_until_dead(pid).await,
        "stdio server PID should exit after stdin EOF"
    );
}

#[tokio::test]
async fn stdio_shutdown_after_tool_call_exits_worker_child() {
    let project = std::env::var_os("LEAN_HOST_MCP_STDIO_TEST_PROJECT")
        .map(PathBuf::from)
        .expect("run scripts/test-installed-stdio.sh to provision the private worker");
    let registry = tempfile::tempdir().expect("temp registry");
    let mut server = StdioServer::start(&project, registry.path());
    server.initialize().await;
    let response = server
        .request_allow_error(
            "tools/call",
            json!({
                "name": "lean_lookup",
                "arguments": {
                    "kind": "declaration",
                    "name": "Nat.add_zero",
                    "imports": ["Init"]
                }
            }),
        )
        .await;

    assert!(
        response.get("error").is_none(),
        "installed worker lookup failed: {response}"
    );
    let result: Value = serde_json::from_str(
        response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("tool envelope"),
    )
    .expect("semantic JSON response");
    assert_eq!(result.get("errors"), Some(&json!([])));
    assert_eq!(result.pointer("/data/status"), Some(&json!("found")));
    assert_eq!(result.pointer("/data/declaration/name"), Some(&json!("Nat.add_zero")));
    // Only the installed child can provide this receipt after opening its
    // checked Lean session. This isolated server has one request and one worker.
    let loaded = result
        .pointer("/trust/native_compatibility/loaded")
        .and_then(Value::as_array)
        .expect("loaded worker handshake");
    let [receipt] = loaded.as_slice() else {
        panic!("expected one loaded generation")
    };
    assert_eq!(receipt.get("project_root").and_then(Value::as_str), project.to_str());
    assert_eq!(receipt.get("worker_session_id"), result.pointer("/trust/session_id"));
    let worker_pids = wait_for_child_pids(server.pid()).await;
    assert_eq!(worker_pids.len(), 1, "expected one direct worker after the handshake");

    server.close_stdin_and_wait().await;
    for pid in worker_pids {
        assert!(
            wait_until_dead(pid).await,
            "worker child PID {pid} should exit with stdio parent"
        );
    }
}

#[tokio::test]
async fn stdio_lean_verify_reports_progress() {
    let registry = tempfile::tempdir().expect("temp registry");
    let mut server = StdioServer::start(&fixture_root(), registry.path());
    server.initialize().await;

    let (response, notifications) = server
        .request_with_notifications(
            "tools/call",
            json!({
                "name": "lean_verify",
                "arguments": {
                    "targets": [{
                        "kind": "bogus_group",
                        "file": "LeanRsFixture/ProofActions.lean"
                    }]
                },
                "_meta": { "progressToken": "verify-progress" }
            }),
        )
        .await;

    assert!(response.get("error").is_none(), "MCP request failed: {response:?}");
    let progress = notifications
        .iter()
        .filter(|message| message.get("method").and_then(Value::as_str) == Some("notifications/progress"))
        .collect::<Vec<_>>();
    assert_eq!(progress.len(), 2, "expected start and finish progress: {progress:?}");
    let [start, finish] = progress.as_slice() else {
        panic!("expected exactly two progress notifications: {progress:?}");
    };
    assert!(progress.iter().all(|message| {
        message.pointer("/params/progressToken").and_then(Value::as_str) == Some("verify-progress")
    }));
    assert_eq!(
        start.pointer("/params/message").and_then(Value::as_str),
        Some("Preparing lean_verify")
    );
    assert!(
        finish
            .pointer("/params/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("lean_verify finished")),
        "finish notification should name the completed operation: {finish:?}"
    );

    server.close_stdin_and_wait().await;
}

struct StdioServer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_task: tokio::task::JoinHandle<String>,
    next_id: u64,
}

impl StdioServer {
    fn start(project_root: &Path, registry: &Path) -> Self {
        let config_dir = registry.join("config");
        std::fs::create_dir(&config_dir).expect("private server config directory");
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        // Cargo injects its test library search path. Start the installed app with
        // the ordinary loader environment so it uses its verified Elan rpath.
        for key in ["DYLD_FALLBACK_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
            command.env_remove(key);
        }
        command
            .arg("--lake-root")
            .arg(project_root)
            .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", config_dir.join("native-owners"))
            .env("LEAN_HOST_MCP_CONFIG_DIR", config_dir)
            .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", registry)
            .env("RUST_LOG", "warn");
        if let Some(dir) = std::env::var_os("LEAN_HOST_MCP_STDIO_TEST_WORKERS_DIR") {
            command.env("LEAN_HOST_MCP_WORKERS_DIR", dir);
        }
        Self::spawn(&mut command)
    }

    fn spawn(command: &mut Command) -> Self {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn lean-host-mcp stdio server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let stderr = child.stderr.take().expect("server stderr");
        let stderr_task = tokio::spawn(read_stderr(stderr));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr_task,
            next_id: 1,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id().expect("server pid")
    }

    async fn initialize(&mut self) {
        self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "lean-host-mcp-stdio-lifecycle", "version": "0.0.0" }
            }),
        )
        .await;
        self.notify("notifications/initialized", json!({})).await;
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.request_allow_error(method, params).await;
        assert!(response.get("error").is_none(), "MCP request failed: {response:?}");
        response
    }

    async fn request_allow_error(&mut self, method: &str, params: Value) -> Value {
        self.request_with_notifications(method, params).await.0
    }

    async fn request_with_notifications(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("request id overflow");
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_message(&message).await;
        let mut notifications = Vec::new();
        loop {
            let response = self.read_response().await;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return (response, notifications);
            }
            notifications.push(response);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.write_message(&message).await;
    }

    async fn write_message(&mut self, message: &Value) {
        let mut encoded = serde_json::to_vec(message).expect("serialize request");
        encoded.push(b'\n');
        let stdin = self.stdin.as_mut().expect("server stdin still open");
        stdin.write_all(&encoded).await.expect("write request");
        stdin.flush().await.expect("flush request");
    }

    async fn read_response(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut line = String::new();
            let bytes = self.stdout.read_line(&mut line).await.expect("read response");
            assert!(bytes > 0, "server stdout closed before response");
            serde_json::from_str(line.trim_end()).expect("parse response")
        })
        .await
        .expect("timed out waiting for MCP response")
    }

    async fn close_stdin_and_wait(mut self) {
        drop(self.stdin.take());
        match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
            Ok(Ok(status)) => assert!(status.success(), "server exit status: {status}"),
            Ok(Err(err)) => panic!("server wait failed: {err}"),
            Err(_) => {
                let pid = self.pid();
                self.child.kill().await.expect("kill exact server child");
                panic!("server PID {pid} did not exit after stdin EOF");
            }
        }
        let stderr = self.stderr_task.await.unwrap_or_default();
        assert!(
            !stderr.contains("panicked"),
            "server stderr should not contain panic output:\n{stderr}"
        );
    }
}

async fn read_stderr(stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut out = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => return out,
            Ok(_) => out.push_str(&line),
            Err(err) => return format!("{out}\nstderr read error: {err}"),
        }
    }
}

fn doctor_processes(registry: &Path) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .args(["doctor", "processes"])
        .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", registry)
        .output()
        .expect("run doctor processes");
    assert!(output.status.success(), "doctor status: {}", output.status);
    String::from_utf8(output.stdout).expect("doctor stdout utf8")
}

async fn wait_for_doctor_pid(registry: &Path, pid: u32) -> String {
    let needle = format!("pid={pid}");
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    for _ in 0..50 {
        tick.tick().await;
        let doctor = doctor_processes(registry);
        if doctor.contains(&needle) {
            return doctor;
        }
    }
    doctor_processes(registry)
}

async fn wait_for_child_pids(parent: u32) -> Vec<u32> {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    for _ in 0..50 {
        tick.tick().await;
        let children = child_pids(parent);
        if !children.is_empty() {
            return children;
        }
    }
    Vec::new()
}

async fn wait_until_dead(pid: u32) -> bool {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    for _ in 0..100 {
        tick.tick().await;
        if !process_alive(pid) {
            return true;
        }
    }
    false
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has workspace parent")
        .parent()
        .expect("workspace has repository parent")
        .join("fixtures/lean")
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn child_pids(parent: u32) -> Vec<u32> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .expect("ps child lookup");
    assert!(output.status.success(), "ps child lookup status: {}", output.status);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

#[cfg(not(unix))]
fn child_pids(_parent: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
mod launcher_regressions {
    use super::*;
    use std::fs;

    const PACKAGED_LAUNCHER: &str = "LEAN_HOST_MCP_STDIO_PACKAGED_LAUNCHER";

    struct Fixture {
        scratch: tempfile::TempDir,
        project: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = tempfile::tempdir().expect("private launcher fixture");
            let project = scratch
                .path()
                .canonicalize()
                .expect("canonical fixture")
                .join("project");
            fs::create_dir(&project).expect("project directory");
            fs::write(
                project.join("lakefile.lean"),
                "import Lake\nopen Lake DSL\npackage Probe\nlean_lib Probe\n",
            )
            .expect("project file");
            fs::write(project.join("lean-toolchain"), "leanprover/lean4:v4.36.0\n").expect("toolchain");
            fs::write(
                project.join("lake-manifest.json"),
                json!({"version": "1.2.0", "packagesDir": ".lake/packages",
                    "name": "Probe", "lakeDir": ".lake", "packages": []})
                .to_string(),
            )
            .expect("empty Lake manifest");
            fs::create_dir_all(scratch.path().join("config/lean-host-mcp")).expect("private config");
            Self { scratch, project }
        }

        fn command(&self) -> Command {
            let parent =
                std::env::var_os(PACKAGED_LAUNCHER).unwrap_or_else(|| env!("CARGO_BIN_EXE_lean-host-mcp").into());
            let mut command = Command::new(parent);
            // Cargo's test loader paths do not belong to the packaged parent.
            command
                .env_remove("DYLD_FALLBACK_LIBRARY_PATH")
                .env_remove("LD_LIBRARY_PATH");
            command
                .current_dir(&self.project)
                .env_remove("LEAN_HOST_MCP_PROJECT")
                .env_remove("LEAN_HOST_MCP_BIND")
                .env_remove("LEAN_HOST_MCP_HTTP_PATH")
                .env_remove("LEAN_HOST_MCP_REQUEST_JOURNAL")
                .env("LEAN_HOST_MCP_CONFIG_DIR", self.scratch.path().join("config"))
                .env(
                    "LEAN_HOST_MCP_PROCESS_REGISTRY_DIR",
                    self.scratch.path().join("processes"),
                )
                .env("LEAN_RS_WORKER_OWNERS_DIRECTORY", self.project.join("owners"))
                .kill_on_drop(true);
            command
        }

        async fn new_store(&self, directory: &str) -> (PathBuf, uuid::Uuid) {
            use std::os::unix::fs::DirBuilderExt as _;
            let path = self.project.join(directory);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .expect("private store parent");
            let path = path.join("stages.sqlite3");
            let mut command = self.command();
            command
                .arg("--lake-root")
                .arg(&self.project)
                .arg("--managed-native-store")
                .arg(&path);
            let mut server = StdioServer::spawn(&mut command);
            server.initialize().await;
            let reply = server
                .request(
                    "tools/call",
                    json!({"name": "lean_managed_admission", "arguments": {
                        "request": json!({"version": 1, "command": {"op": "discover"}}).to_string()
                    }}),
                )
                .await;
            let data: Value = serde_json::from_str(
                reply
                    .pointer("/result/content/0/text")
                    .and_then(Value::as_str)
                    .expect("discover reply"),
            )
            .expect("discover JSON");
            let bytes: [u8; 16] = serde_json::from_value(data.get("store").expect("discover store identity").clone())
                .expect("store UUID bytes");
            server.close_stdin_and_wait().await;
            (path, uuid::Uuid::from_bytes(bytes))
        }

        fn enroll(&self, store: &Path, id: uuid::Uuid) -> PathBuf {
            let config = self.project.join("lean-host-mcp.toml");
            fs::write(
                &config,
                format!(
                    "[launcher]\nmode = \"ordinary\"\ngeneration = 1\nmanaged_store = {:?}\nmanaged_store_uuid = \"{id}\"\n",
                    store.to_str().expect("UTF-8 test path")
                ),
            )
            .expect("enroll project");
            config
        }

        async fn custody(&self) -> PathBuf {
            use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
            let output = self
                .command()
                .arg("writer-lock-path")
                .arg(&self.project)
                .output()
                .await
                .expect("lock domain");
            assert!(output.status.success(), "{output:?}");
            let path: Value = serde_json::from_slice(&output.stdout).expect("lock JSON");
            let path = PathBuf::from(path.get("lock_path").and_then(Value::as_str).expect("lock path"));
            assert!(
                path.starts_with(self.project.parent().expect("private fixture root")),
                "{}",
                path.display()
            );
            fs::create_dir_all(path.parent().expect("lock parent")).expect("lock parent directory");
            let create_lock = |path: &Path| {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .expect("private lock")
            };
            let lock = create_lock(&path).metadata().expect("lock identity");
            let owners = self.project.join("owners");
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&owners)
                .expect("private registry");
            create_lock(&owners.join(".admission.lock"));
            let registry = owners.metadata().expect("registry identity");
            let pins = self.project.join("custody.json");
            fs::write(
                &pins,
                json!({
                    "registry": {"path": owners, "device": registry.dev(), "inode": registry.ino()},
                    "locks": [[path, lock.dev(), lock.ino()]]
                })
                .to_string(),
            )
            .expect("custody proof");
            pins
        }

        fn transition(&self, pins: &Path, from: &str, to: &str) -> Command {
            let mut command = self.command();
            command
                .arg("launcher-transition")
                .arg("--project")
                .arg(&self.project)
                .args(["--from-generation", from, "--to", to])
                .arg("--custody")
                .arg(pins);
            command
        }
    }

    #[tokio::test]
    async fn launcher_cross_project_loaded_request_requires_actual_project_selection() {
        let fixture = Fixture::new();
        let (store, id) = fixture.new_store("store").await;
        fixture.enroll(&store, id);
        fs::write(
            fixture.scratch.path().join("config/lean-host-mcp/config.toml"),
            format!(
                "primary_project = {:?}\n",
                fixture.project.to_str().expect("UTF-8 project")
            ),
        )
        .expect("approve enrolled project");
        let cwd = fixture.scratch.path().join("other");
        fs::create_dir(&cwd).expect("other project");
        fs::write(cwd.join("lean-host-mcp.toml"), "[runtime]\n").expect("unenrolled config");
        let mut server = StdioServer::spawn(fixture.command().current_dir(cwd));
        server.initialize().await;
        let response = server
            .request_allow_error(
                "tools/call",
                json!({
                    "name": "lean_compatibility",
                    "arguments": {"project": fixture.project, "mode": "loaded"}
                }),
            )
            .await;
        let response = response.to_string();
        assert!(response.contains("launcher admission refused"), "{response}");
        assert!(response.contains("enrolled project requires"), "{response}");
        server.close_stdin_and_wait().await;
    }

    async fn managed(server: &mut StdioServer, command: Value) -> Value {
        let reply = server
            .request(
                "tools/call",
                json!({"name":"lean_managed_admission","arguments":{
                    "request":json!({"version":1,"command":command}).to_string()
                }}),
            )
            .await;
        serde_json::from_str(
            reply
                .pointer("/result/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("expected managed reply: {reply}")),
        )
        .expect("managed JSON reply")
    }

    #[tokio::test]
    async fn packaged_managed_build_recovers_original_result_then_rolls_back() {
        if std::env::var_os(PACKAGED_LAUNCHER).is_none() {
            return;
        }
        let writer = std::env::var_os("LEAN_HOST_MCP_STDIO_PACKAGED_WRITER")
            .expect("packaged release supplies its installed writer");
        let fixture = Fixture::new();
        fs::write(fixture.project.join("Probe.lean"), "def packagedRecovery : Nat := 19\n")
            .expect("disposable Lean input");
        let (store, id) = fixture.new_store("store").await;
        let selector = fixture.enroll(&store, id);
        let pins = {
            let pins = fixture.custody().await;
            let outcome = fixture
                .transition(&pins, "1", "managed")
                .output()
                .await
                .expect("cutover");
            assert!(outcome.status.success(), "{outcome:?}");
            pins
        };
        {
            let start_server = || {
                let mut command = fixture.command();
                command
                    .arg("--lake-root")
                    .arg(&fixture.project)
                    .args([
                        "--launcher-mode",
                        "managed",
                        "--launcher-generation",
                        "2",
                        "--managed-only",
                    ])
                    .arg("--managed-native-store")
                    .arg(&store)
                    .env("LEAN_HOST_MCP_MANAGED_READINESS_WRITER", &writer);
                StdioServer::spawn(&mut command)
            };
            let mut first = start_server();
            first.initialize().await;
            let discovered = managed(&mut first, json!({"op":"discover"})).await;
            assert!(
                discovered
                    .get("operations")
                    .and_then(Value::as_array)
                    .expect("operation list")
                    .contains(&json!("lake_module_preparation_v1"))
            );
            let key = json!({"store":discovered.get("store").expect("store"),"epoch":discovered.get("acceptance_epoch").expect("epoch"),
                "nonce":uuid::Uuid::new_v4().into_bytes(),"stage":1});
            let capability = [57_u8; 32];
            let intent = json!({"project":fixture.project,"job":[1],"revision":1,
                "operation":"lake_module_preparation_v1","target":{"kind":"module","name":"Probe"}});
            let create = managed(
                &mut first,
                json!({"op":"create","key":key,"capability":capability,
                "intent":intent.to_string().as_bytes()}),
            )
            .await;
            assert_eq!(
                create.get("execution").and_then(Value::as_str),
                Some("admitted"),
                "{create}"
            );
            first.close_stdin_and_wait().await;
            let blocked = fixture
                .transition(&pins, "2", "ordinary")
                .output()
                .await
                .expect("rollback with pending admission");
            assert!(!blocked.status.success(), "{blocked:?}");
            assert!(
                String::from_utf8_lossy(&blocked.stderr).contains("unsettled stages"),
                "{blocked:?}"
            );

            let command = |op| json!({"op":op,"key":key,"capability":capability});
            let mut dispatched = start_server();
            dispatched.initialize().await;
            let completed = managed(&mut dispatched, command("start")).await;
            assert_eq!(
                completed.get("custody").and_then(Value::as_str),
                Some("closed"),
                "{completed}"
            );
            let body: Vec<u8> = serde_json::from_value(completed.pointer("/result/body").expect("result body").clone())
                .expect("retained body");
            let result: Value = serde_json::from_slice(&body).expect("build receipt");
            assert_eq!(
                result.get("build_exit_code").and_then(Value::as_i64),
                Some(0),
                "{result}"
            );
            dispatched.close_stdin_and_wait().await;
            let olean = fixture.project.join(".lake/build/lib/lean/Probe.olean");
            assert!(olean.is_file());
            fs::remove_file(&olean).expect("remove disposable build output to witness no replay");

            let mut replacement = start_server();
            replacement.initialize().await;
            assert_eq!(managed(&mut replacement, command("get")).await, completed);
            assert_eq!(managed(&mut replacement, command("start")).await, completed);
            assert!(!olean.exists(), "original build replayed");
            let ack = managed(
                &mut replacement,
                json!({"op":"ack","key":key,"capability":capability,
                "result_id":completed.pointer("/result/id").expect("result ID"),"result_digest":completed.pointer("/result/sha256").expect("result digest")}),
            )
            .await;
            assert_eq!(
                ack.pointer("/result/state").and_then(Value::as_str),
                Some("expired"),
                "{ack}"
            );
            replacement.close_stdin_and_wait().await;

            let rollback = fixture
                .transition(&pins, "2", "ordinary")
                .output()
                .await
                .expect("settled rollback");
            assert!(rollback.status.success(), "{rollback:?}");
            let config: toml::Table =
                toml::from_str(&fs::read_to_string(selector).expect("read selector")).expect("selector TOML");
            let launcher = config
                .get("launcher")
                .and_then(toml::Value::as_table)
                .expect("launcher table");
            assert_eq!(launcher.get("mode").and_then(toml::Value::as_str), Some("ordinary"));
            assert_eq!(launcher.get("generation").and_then(toml::Value::as_integer), Some(3));
            let mut ordinary = StdioServer::spawn(fixture.command().arg("--lake-root").arg(&fixture.project).args([
                "--launcher-mode",
                "ordinary",
                "--launcher-generation",
                "3",
            ]));
            ordinary.initialize().await;
            ordinary.close_stdin_and_wait().await;
        }
    }

    #[tokio::test]
    async fn managed_root_library_batches_guard_cold_external_inputs() {
        let writer = std::env::var_os("LEAN_HOST_MCP_STDIO_PACKAGED_WRITER");
        let Some(writer) = writer else { return };
        let fixture = Fixture::new();
        let dep = fixture.project.join("ext");
        fs::create_dir(&dep).expect("external package");
        fs::create_dir(fixture.project.join("Probe")).expect("module directory");
        for (path, source) in [
            (
                "lakefile.lean",
                "import Lake\nopen Lake DSL\npackage Probe\nrequire ext from \"./ext\"\nlean_lib Probe\n",
            ),
            (
                "ext/lakefile.lean",
                "import Lake\nopen Lake DSL\npackage ext\nlean_lib Ext\n",
            ),
            ("ext/Ext.lean", "def extValue : Nat := 42\n"),
            ("Probe/A.lean", "import Ext\ndef a : Nat := extValue\n"),
            ("Probe/B.lean", "def b : Nat := 5\n"),
            ("Probe.lean", "import Probe.A\nimport Probe.B\n"),
        ] {
            fs::write(fixture.project.join(path), source).expect("disposable source");
        }
        let lake = PathBuf::from(std::env::var_os("LEAN_SYSROOT").expect("fixture toolchain")).join("bin/lake");
        let lake_command = |cwd: &std::path::Path, args: &[&str]| {
            let output = std::process::Command::new(&lake)
                .current_dir(cwd)
                .arg("--no-cache")
                .args(args)
                .output()
                .expect("fixture Lake");
            assert!(output.status.success(), "{output:?}");
        };
        lake_command(&dep, &["update"]);
        lake_command(&fixture.project, &["update"]);
        let (store, _) = fixture.new_store("store").await;
        let mut server = StdioServer::spawn(
            fixture
                .command()
                .arg("--lake-root")
                .arg(&fixture.project)
                .arg("--managed-native-store")
                .arg(store)
                .env("LEAN_HOST_MCP_MANAGED_READINESS_WRITER", writer),
        );
        server.initialize().await;
        let info = managed(&mut server, json!({"op":"discover"})).await;
        let capability = [57_u8; 32];
        let ext_object = dep.join(".lake/build/lib/lean/Ext.olean");
        let root_object = fixture.project.join(".lake/build/lib/lean/Probe.olean");
        use sha2::{Digest, Sha256};
        let digest: [u8; 32] = Sha256::digest(b"Probe\0Probe.A\0Probe.B\0").into();
        for (revision, expected_modules) in [(1, 0), (2, 0), (3, 3)] {
            if revision == 2 {
                lake_command(&fixture.project, &["build", "@ext/+Ext:irArts"]);
            }
            let before = ext_object.metadata().and_then(|meta| meta.modified()).ok();
            assert!(!root_object.exists());
            let key = json!({"store":info.get("store").expect("store"), "epoch":info.get("acceptance_epoch").expect("epoch"),
                "nonce":uuid::Uuid::new_v4().into_bytes(), "stage":1});
            let intent = json!({"project":fixture.project, "job":[revision], "revision":revision,
                "operation":"lake_module_preparation_v1", "target":{"kind":"root_library","name":"Probe",
                    "module_count":3, "module_names_sha256":digest}});
            let create = managed(
                &mut server,
                json!({"op":"create","key":key,
                "capability":capability,"intent":intent.to_string().as_bytes()}),
            )
            .await;
            assert_eq!(
                create.get("execution").and_then(Value::as_str),
                Some("admitted"),
                "{create}"
            );
            let module_b = fixture.project.join("Probe/B.lean");
            let module_c = fixture.project.join("Probe/C.lean");
            let source = fixture.project.join("Probe.lean");
            if revision == 2 {
                fs::rename(&module_b, &module_c).expect("substitute one collected module");
                fs::write(&source, "import Probe.A\nimport Probe.C\n").expect("update root imports");
            }
            let completed = managed(&mut server, json!({"op":"start","key":key,"capability":capability})).await;
            assert_eq!(
                completed.get("custody").and_then(Value::as_str),
                Some("closed"),
                "{completed}"
            );
            let body: Vec<u8> = serde_json::from_value(completed.pointer("/result/body").expect("result body").clone())
                .expect("retained result");
            let receipt: Value = serde_json::from_slice(&body).expect("root-library receipt");
            assert_eq!(
                receipt.get("module_count").and_then(Value::as_u64),
                Some(3),
                "{receipt}"
            );
            assert_eq!(
                receipt.get("completed_modules").and_then(Value::as_u64),
                Some(expected_modules),
                "{receipt}"
            );
            if revision == 1 {
                assert_eq!(
                    receipt.pointer("/first_failure/phase").and_then(Value::as_str),
                    Some("dependencies"),
                    "{receipt}"
                );
                assert!(!fixture.project.join(".lake/build/lib/lean/Probe/B.olean").exists());
                assert!(!ext_object.exists());
            } else if revision == 2 {
                assert_eq!(
                    receipt.pointer("/first_failure/phase").and_then(Value::as_str),
                    Some("inventory_mismatch"),
                    "{receipt}"
                );
                for module in ["A", "B", "C"] {
                    assert!(
                        !fixture
                            .project
                            .join(format!(".lake/build/lib/lean/Probe/{module}.olean"))
                            .exists()
                    );
                }
                fs::rename(&module_c, &module_b).expect("restore original module");
                fs::write(&source, "import Probe.A\nimport Probe.B\n").expect("restore root imports");
            } else {
                assert!(receipt.get("first_failure").is_some_and(Value::is_null), "{receipt}");
                assert_eq!(
                    receipt.get("completed_batches").and_then(Value::as_u64),
                    Some(2),
                    "{receipt}"
                );
                assert!(root_object.is_file());
                fs::remove_file(&root_object).expect("remove disposable object to witness no replay");
            }
            assert_eq!(ext_object.metadata().and_then(|meta| meta.modified()).ok(), before);
            assert_eq!(
                managed(&mut server, json!({"op":"get","key":key,"capability":capability})).await,
                completed
            );
            assert_eq!(
                managed(&mut server, json!({"op":"start","key":key,"capability":capability})).await,
                completed
            );
            assert!(!root_object.exists());
        }
        server.close_stdin_and_wait().await;
    }

    #[tokio::test]
    async fn launcher_rejects_valid_replacement_store_at_original_path() {
        let fixture = Fixture::new();
        let (store, id) = fixture.new_store("store").await;
        let (substitute, other_id) = fixture.new_store("substitute").await;
        assert_ne!(id, other_id);
        let config = fixture.enroll(&store, id);
        let pins = {
            let pins = fixture.custody().await;
            let output = fixture
                .transition(&pins, "1", "managed")
                .output()
                .await
                .expect("switch to managed");
            assert!(output.status.success(), "{output:?}");
            pins
        };
        let before = fs::read(&config).expect("original selector");
        let saved = store.with_extension("original");
        fs::rename(&store, &saved).expect("save original store");
        fs::rename(substitute, &store).expect("install empty substitute");
        let mut startup = fixture.command();
        startup.arg("--lake-root").arg(&fixture.project);
        startup
            .args([
                "--launcher-mode",
                "managed",
                "--launcher-generation",
                "2",
                "--managed-only",
            ])
            .arg("--managed-native-store")
            .arg(&store);
        let output = startup.output().await.expect("startup rejection");
        assert!(!output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("original launcher store UUID changed"),
            "{output:?}"
        );
        {
            let output = fixture
                .transition(&pins, "2", "ordinary")
                .output()
                .await
                .expect("rollback rejection");
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("original launcher store UUID changed"),
                "{output:?}"
            );
        }
        assert_eq!(fs::read(config).expect("selector after refusal"), before);
    }

    #[tokio::test]
    async fn launcher_uncertain_publication_reconciles_without_another_generation() {
        use std::os::unix::fs::MetadataExt as _;
        let fixture = Fixture::new();
        let (store, id) = fixture.new_store("store").await;
        let config = fixture.enroll(&store, id);
        let pins = fixture.custody().await;
        let cut = fixture
            .transition(&pins, "1", "managed")
            .env("LEAN_HOST_MCP_TEST_LAUNCHER_RENAME_CUT", "1")
            .output()
            .await
            .expect("rename cut");
        assert!(!cut.status.success(), "{cut:?}");
        assert!(
            String::from_utf8_lossy(&cut.stderr).contains("disposable cut after selector rename"),
            "{cut:?}"
        );
        let visible = fs::read_to_string(&config).expect("visible selector");
        let selector: toml::Table = toml::from_str(&visible).expect("selector TOML");
        let launcher = selector
            .get("launcher")
            .and_then(toml::Value::as_table)
            .expect("launcher table");
        assert_eq!(launcher.get("generation").and_then(toml::Value::as_integer), Some(2));
        assert_eq!(launcher.get("mode").and_then(toml::Value::as_str), Some("managed"));
        let inode = config.metadata().expect("visible inode").ino();

        let retry = fixture
            .transition(&pins, "1", "managed")
            .output()
            .await
            .expect("exact retry");
        assert!(retry.status.success(), "{retry:?}");
        assert_eq!(fs::read_to_string(&config).expect("reconciled selector"), visible);
        assert_eq!(config.metadata().expect("reconciled inode").ino(), inode);
        let mut server = StdioServer::spawn(
            fixture
                .command()
                .arg("--lake-root")
                .arg(&fixture.project)
                .args([
                    "--launcher-mode",
                    "managed",
                    "--launcher-generation",
                    "2",
                    "--managed-only",
                ])
                .arg("--managed-native-store")
                .arg(&store),
        );
        server.initialize().await;
        server.close_stdin_and_wait().await;
    }
}
