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
    if !debug_worker_binary().is_file() {
        eprintln!(
            "skipping worker-child lifecycle check: {} is missing",
            debug_worker_binary().display()
        );
        return;
    }

    let registry = tempfile::tempdir().expect("temp registry");
    let mut server = StdioServer::start(&fixture_root(), registry.path());
    server.initialize().await;
    server
        .request_allow_error(
            "tools/call",
            json!({
                "name": "lean_lookup",
                "arguments": {
                    "kind": "declaration",
                    "name": "Nat.add_zero",
                    "imports": ["LeanRsFixture.Handles"]
                }
            }),
        )
        .await;

    let worker_pids = wait_for_child_pids(server.pid()).await;
    assert!(
        !worker_pids.is_empty(),
        "tool call should leave a direct worker child for exact-PID shutdown assertion"
    );

    server.close_stdin_and_wait().await;
    for pid in worker_pids {
        assert!(
            wait_until_dead(pid).await,
            "worker child PID {pid} should exit with stdio parent"
        );
    }
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
        let config_dir = tempfile::tempdir().expect("temp config dir").keep();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        command
            .arg("--lake-root")
            .arg(project_root)
            .env("LEAN_HOST_MCP_CONFIG_DIR", config_dir)
            .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", registry)
            .env("LEAN_HOST_MCP_WORKERS_DIR", debug_workers_dir())
            .env("RUST_LOG", "warn")
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
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("request id overflow");
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_message(&message).await;
        loop {
            let response = self.read_response().await;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return response;
            }
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

fn debug_workers_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has workspace parent")
        .parent()
        .expect("workspace has repository parent")
        .join("target/debug")
}

fn debug_worker_binary() -> PathBuf {
    debug_workers_dir().join("lean-host-mcp-worker")
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
