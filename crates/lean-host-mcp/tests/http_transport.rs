//! Black-box Streamable HTTP transport tests.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStderr, Command};

const ACCEPT_BOTH: &str = "application/json, text/event-stream";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const EXPECTED_TOOLS: [&str; 5] = [
    "lean_context",
    "lean_lookup",
    "lean_status",
    "lean_trial",
    "lean_verify",
];
const REMOVED_TOOLS: [&str; 6] = [
    "proof_state",
    "search_for_proof",
    "inspect_declaration",
    "try_proof_step",
    "verify_declaration",
    "find_references",
];

#[tokio::test]
async fn streamable_http_initialize_and_tools_list() {
    let server = HttpMcpServer::start(&[]).await;
    let mut session = server.new_session().await;

    let tools = session
        .request("tools/list", json!({}))
        .await
        .expect("tools/list response");
    let names = tool_names(&tools.json);
    assert_eq!(names, expected_tools());
    for removed in REMOVED_TOOLS {
        assert!(
            !names.contains(removed),
            "old public tool {removed} must not be advertised"
        );
    }

    // Handlers return a bare `CallToolResult`, so rmcp advertises no `outputSchema`.
    // Keep tool input schemas in the narrow object/properties/required subset that
    // strict MCP clients reliably ingest.
    let listed = tools
        .json
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools/list should carry a tools array");
    for tool in listed {
        assert!(
            tool.get("outputSchema").is_none(),
            "tool {} should not advertise an outputSchema: {tool:?}",
            tool.get("name").and_then(Value::as_str).unwrap_or("?")
        );
        assert_eq!(
            tool.pointer("/inputSchema/type").and_then(Value::as_str),
            Some("object"),
            "tool {} must advertise an object inputSchema for strict MCP clients: {tool:?}",
            tool.get("name").and_then(Value::as_str).unwrap_or("?")
        );
        assert!(
            tool.pointer("/inputSchema/properties")
                .and_then(Value::as_object)
                .is_some(),
            "tool {} must advertise top-level inputSchema properties for strict MCP clients: {tool:?}",
            tool.get("name").and_then(Value::as_str).unwrap_or("?")
        );
        assert!(
            tool.pointer("/inputSchema/oneOf").is_none(),
            "tool {} must not advertise root-level oneOf; strict MCP clients may hide it: {tool:?}",
            tool.get("name").and_then(Value::as_str).unwrap_or("?")
        );
    }
    let verify_tool = listed
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("lean_verify"))
        .expect("lean_verify should be listed");
    let verify_schema = verify_tool
        .get("inputSchema")
        .expect("lean_verify should advertise an inputSchema");
    assert!(
        verify_schema.pointer("/properties/targets").is_some(),
        "lean_verify inputSchema must advertise the target-group request shape: {verify_schema:?}"
    );
    assert!(
        verify_schema.pointer("/properties/kind").is_none(),
        "lean_verify has no public kind namespace and must not advertise the generic semantic schema: {verify_schema:?}"
    );
    let lookup_schema = listed
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("lean_lookup"))
        .and_then(|tool| tool.get("inputSchema"))
        .expect("lean_lookup should advertise an inputSchema");
    let lookup_schema_text = lookup_schema.to_string();
    assert_eq!(
        lookup_schema
            .pointer("/properties/kind/enum")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(5),
        "lean_lookup top-level kind property should summarize every lookup mode: {lookup_schema:?}"
    );
    assert!(
        lookup_schema.pointer("/properties/target").is_some(),
        "lean_lookup top-level properties should expose the declarations target shape: {lookup_schema:?}"
    );
    for expected in ["declarations", "target", "module", "path"] {
        assert!(
            lookup_schema_text.contains(expected),
            "lean_lookup schema should expose {expected:?}: {lookup_schema:?}"
        );
    }
    assert!(
        !lookup_schema_text.contains("oneOf") && !lookup_schema_text.contains("$defs"),
        "lean_lookup schema should avoid compatibility-sensitive JSON Schema combinators: {lookup_schema:?}"
    );
    let context_schema = listed
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("lean_context"))
        .and_then(|tool| tool.get("inputSchema"))
        .expect("lean_context should advertise an inputSchema");
    assert!(
        context_schema.pointer("/properties/declaration").is_some(),
        "lean_context top-level properties should expose declaration: {context_schema:?}"
    );
    assert!(
        context_schema.to_string().contains("proof_position"),
        "lean_context schema should expose proof_position examples: {context_schema:?}"
    );
    let trial_schema = listed
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("lean_trial"))
        .and_then(|tool| tool.get("inputSchema"))
        .expect("lean_trial should advertise an inputSchema");
    assert!(
        trial_schema.pointer("/properties/commands").is_some(),
        "lean_trial top-level properties should expose command-trial fields: {trial_schema:?}"
    );
    let status_schema = listed
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("lean_status"))
        .and_then(|tool| tool.get("inputSchema"))
        .expect("lean_status should advertise an inputSchema");
    assert!(
        status_schema.pointer("/properties/include").is_some(),
        "lean_status top-level properties should expose project-status fields: {status_schema:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn invalid_http_startup_config_exits() {
    for args in [
        vec!["--http-path", "/mcp"],
        vec!["--bind", "0.0.0.0:8765"],
        vec!["--bind", "127.0.0.1:8765", "--http-path", "mcp"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
            .args(args)
            .env(
                "LEAN_HOST_MCP_CONFIG_DIR",
                tempfile::tempdir().expect("temp config dir").keep(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .expect("run invalid startup command");
        assert!(
            !output.status.success(),
            "invalid startup command should fail: {:?}",
            output.status
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("lean-host-mcp exited with error"),
            "stderr should include startup failure, got: {stderr}"
        );
    }
}

#[tokio::test]
async fn lean_verify_invalid_target_group_is_structured_tool_error() {
    let server = HttpMcpServer::start(&[]).await;
    let mut session = server.new_session().await;

    let response = session
        .request(
            "tools/call",
            json!({
                "name": "lean_verify",
                "arguments": {
                    "targets": [{
                        "kind": "bogus_group",
                        "file": "LeanRsFixture/ProofActions.lean"
                    }]
                }
            }),
        )
        .await
        .expect("invalid lean_verify target group");

    assert!(response.http_status.is_success());
    assert!(
        response.json.get("error").is_none(),
        "invalid lean_verify target group should not be a JSON-RPC error: {:?}",
        response.json
    );
    assert_eq!(semantic_error_code(&response.json).as_deref(), Some("invalid_request"));
    assert_eq!(
        response
            .json
            .pointer("/result/structuredContent/errors/0/details/example/targets/0/kind")
            .and_then(Value::as_str),
        Some("explicit")
    );

    server.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn streamable_http_sigterm_shutdown_exits() {
    let server = HttpMcpServer::start(&[]).await;
    let pid = server.child.id().expect("server pid");
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("send SIGTERM");
    assert!(status.success(), "kill -TERM should succeed: {status}");
    server.wait_for_exit().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_fixture_tool_call() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    let server = HttpMcpServer::start(&[("--lake-root", root_s.as_str())]).await;
    let mut session = server.new_session().await;

    let response = session
        .request(
            "tools/call",
            json!({
                "name": "lean_context",
                "arguments": {
                    "kind": "proof_position",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.stepTheorem"
                }
            }),
        )
        .await
        .expect("lean_context over HTTP");

    assert!(
        response.http_status.is_success(),
        "runtime results must not be HTTP errors: {response:?}"
    );
    assert!(
        response.json.get("error").is_none(),
        "lean_context must not return JSON-RPC error: {:?}",
        response.json
    );
    assert_eq!(envelope_status(&response.json), "ok");

    let invalid = session
        .request(
            "tools/call",
            json!({
                "name": "lean_context",
                "arguments": {
                    "kind": "not_a_context_mode",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.stepTheorem"
                }
            }),
        )
        .await
        .expect("invalid lean_context mode");
    assert!(invalid.http_status.is_success());
    assert!(
        invalid.json.get("error").is_none(),
        "invalid semantic mode should not be a JSON-RPC error: {:?}",
        invalid.json
    );
    assert_eq!(semantic_error_code(&invalid.json).as_deref(), Some("invalid_kind"));

    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_semantic_admission_concurrent_sessions_surface_structured_pressure() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_str())],
        &[
            ("LEAN_HOST_MCP_SEMANTIC_PERMITS", "1"),
            ("LEAN_HOST_MCP_SEMANTIC_WAITERS", "1"),
            ("LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS", "1"),
        ],
    )
    .await;
    let mut sessions = Vec::new();
    for _ in 0..6 {
        sessions.push(server.new_session().await);
    }

    let calls = fixture_calls();
    let mut tasks = Vec::new();
    for (index, mut session) in sessions.into_iter().enumerate() {
        let (name, arguments) = calls[index % calls.len()].clone();
        tasks.push(tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({
                        "name": name,
                        "arguments": arguments,
                    }),
                )
                .await
        }));
    }

    let mut saw_pressure = false;
    let mut saw_wait = false;
    let mut outcomes = BTreeMap::<String, usize>::new();
    for task in tasks {
        let response = task.await.expect("HTTP task join").expect("HTTP tool response");
        assert!(
            response.http_status.is_success(),
            "runtime pressure must not be an HTTP error: {response:?}"
        );
        assert!(
            response.json.get("error").is_none(),
            "runtime pressure must not be a JSON-RPC error: {:?}",
            response.json
        );
        match envelope_status(&response.json).as_str() {
            "ok" => {
                *outcomes.entry("ok".to_owned()).or_default() += 1;
                saw_wait |= runtime_u64(&response.json, "admission_wait_millis").unwrap_or_default() > 0;
                saw_wait |= runtime_u64(&response.json, "queue_wait_millis").unwrap_or_default() > 0;
            }
            "runtime_unavailable" => {
                let reason = runtime_error_reason(&response.json).unwrap_or_default();
                assert!(
                    matches!(
                        reason.as_str(),
                        "semantic_admission_full" | "semantic_admission_timeout" | "mailbox_full"
                    ),
                    "unexpected runtime_unavailable reason {reason}: {:?}",
                    response.json
                );
                *outcomes.entry(reason).or_default() += 1;
                saw_pressure = true;
            }
            other => panic!("unexpected envelope status {other}: {:?}", response.json),
        }
    }
    assert!(
        saw_pressure || saw_wait,
        "concurrent HTTP sessions should reach broker admission/queue pressure"
    );
    println!(
        "{}",
        json!({
            "event": "streamable_http_concurrency",
            "saw_pressure": saw_pressure,
            "saw_wait": saw_wait,
            "outcomes": outcomes,
        })
    );

    let mut follow_up = server.new_session().await;
    let response = follow_up
        .request(
            "tools/call",
            json!({
                "name": "lean_context",
                "arguments": {
                    "kind": "proof_position",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.stepTheorem"
                }
            }),
        )
        .await
        .expect("healthy follow-up lean_context");
    assert!(response.http_status.is_success());
    assert!(response.json.get("error").is_none());

    server.shutdown().await;
}

#[derive(Debug)]
struct HttpMcpServer {
    child: Child,
    stderr_task: tokio::task::JoinHandle<String>,
    client: reqwest::Client,
    url: String,
}

impl HttpMcpServer {
    async fn start(args: &[(&str, &str)]) -> Self {
        Self::start_with_env(args, &[]).await
    }

    async fn start_with_env(args: &[(&str, &str)], envs: &[(&str, &str)]) -> Self {
        let bind = reserve_loopback_addr();
        let config_dir = tempfile::tempdir().expect("temp config dir").keep();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        command
            .arg("--bind")
            .arg(bind.to_string())
            .env("LEAN_HOST_MCP_CONFIG_DIR", config_dir)
            .env("RUST_LOG", "warn")
            // These tests read `structuredContent` and full telemetry; opt into both.
            .env("LEAN_HOST_MCP_RESPONSE_CARRIER", "both")
            .env("LEAN_HOST_MCP_TELEMETRY_VERBOSITY", "full")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (key, value) in args {
            command.arg(key).arg(value);
        }
        for (key, value) in envs {
            command.env(key, value);
        }

        let mut child = command.spawn().expect("spawn lean-host-mcp HTTP server");
        let stderr = child.stderr.take().expect("server stderr");
        let stderr_task = tokio::spawn(read_stderr(stderr));
        wait_for_listen(bind).await;

        Self {
            child,
            stderr_task,
            client: reqwest::Client::new(),
            url: format!("http://{bind}/mcp"),
        }
    }

    async fn new_session(&self) -> HttpSession {
        let mut session = HttpSession {
            client: self.client.clone(),
            url: self.url.clone(),
            session_id: None,
            next_id: 1,
        };
        session.initialize().await;
        session
    }

    async fn shutdown(mut self) {
        if let Err(err) = self.child.start_kill() {
            eprintln!("HTTP test server kill failed: {err}");
        }
        drop(tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await);
        self.finish().await;
    }

    async fn wait_for_exit(mut self) {
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("HTTP test server should exit")
            .expect("wait for HTTP test server");
        assert!(status.success(), "HTTP test server exit status: {status}");
        self.finish().await;
    }

    async fn finish(self) {
        let stderr = self.stderr_task.await.unwrap_or_default();
        if !stderr.trim().is_empty() {
            eprintln!("HTTP test server stderr:\n{stderr}");
        }
    }
}

#[derive(Debug)]
struct HttpSession {
    client: reqwest::Client,
    url: String,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpSession {
    async fn initialize(&mut self) {
        let response = self
            .post_json(json!({
                "jsonrpc": "2.0",
                "id": self.next_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "lean-host-mcp-http-test", "version": "0.0.0" }
                }
            }))
            .await
            .expect("initialize response");
        self.next_id = self.next_id.checked_add(1).expect("request id overflow");
        assert!(
            response.http_status.is_success(),
            "initialize HTTP status: {response:?}"
        );
        assert!(
            response.json.get("error").is_none(),
            "initialize JSON-RPC error: {response:?}"
        );
        self.session_id = response.session_id;
        assert!(self.session_id.is_some(), "initialize should return Mcp-Session-Id");

        let notify = self
            .raw_post(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
            }))
            .await
            .expect("notifications/initialized response");
        assert_eq!(notify.status().as_u16(), 202);
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<HttpMcpResponse, String> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("request id overflow");
        let response = self
            .post_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await?;
        let response_id = response.json.get("id").and_then(Value::as_u64);
        if response_id != Some(id) {
            return Err(format!(
                "expected response id {id}, got {response_id:?}: {:?}",
                response.json
            ));
        }
        Ok(response)
    }

    async fn post_json(&self, body: Value) -> Result<HttpMcpResponse, String> {
        let response = self.raw_post(body).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let session_id = headers
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = response.bytes().await.map_err(|err| format!("read HTTP body: {err}"))?;
        let json = parse_response_body(&headers, &bytes)?;
        Ok(HttpMcpResponse {
            http_status: status,
            session_id,
            json,
        })
    }

    async fn raw_post(&self, body: Value) -> Result<reqwest::Response, String> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", ACCEPT_BOTH)
            .header("Content-Type", "application/json")
            .body(body.to_string());
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        request.send().await.map_err(|err| format!("HTTP POST: {err}"))
    }
}

#[derive(Debug)]
struct HttpMcpResponse {
    http_status: reqwest::StatusCode,
    session_id: Option<String>,
    json: Value,
}

fn reserve_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    let addr = listener.local_addr().expect("reserved addr");
    drop(listener);
    addr
}

async fn wait_for_listen(addr: SocketAddr) {
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(5))
        .expect("deadline should fit in tokio Instant range");
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "HTTP server did not start listening at {addr}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn read_stderr(stderr: ChildStderr) -> String {
    let mut reader = BufReader::new(stderr);
    let mut out = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => return out,
            Ok(_) => out.push_str(&line),
            Err(err) => return format!("{out}\nstderr read error: {err}"),
        }
    }
}

fn parse_response_body(headers: &HeaderMap, bytes: &[u8]) -> Result<Value, String> {
    let body = std::str::from_utf8(bytes).map_err(|err| format!("HTTP body is not UTF-8: {err}"))?;
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if content_type.contains("application/json") {
        return serde_json::from_str(body).map_err(|err| format!("parse JSON response: {err}: {body}"));
    }
    if content_type.contains("text/event-stream") {
        return parse_sse_json(body);
    }
    Err(format!("unsupported response content type {content_type:?}: {body}"))
}

fn parse_sse_json(body: &str) -> Result<Value, String> {
    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<Value>(&data) {
            return Ok(json);
        }
    }
    Err(format!("no JSON data event in SSE body: {body}"))
}

fn expected_tools() -> BTreeSet<String> {
    EXPECTED_TOOLS.into_iter().map(ToOwned::to_owned).collect()
}

fn tool_names(response: &Value) -> BTreeSet<String> {
    response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn fixture_root() -> PathBuf {
    std::env::var_os("LEAN_HOST_MCP_TEST_FIXTURE")
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("crate has workspace parent")
                .parent()
                .expect("workspace has repository parent")
                .join("fixtures/lean")
        })
}

fn fixture_calls() -> Vec<(&'static str, Value)> {
    vec![
        (
            "lean_context",
            json!({
                "kind": "proof_position",
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.stepTheorem"
            }),
        ),
        (
            "lean_lookup",
            json!({
                "kind": "declaration",
                "name": "LeanRsFixture.SourceRanges.knownTheorem",
                "file": "LeanRsFixture/SourceRanges.lean"
            }),
        ),
        (
            "lean_verify",
            json!({
                "targets": [{
                    "kind": "explicit",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declarations": ["LeanRsFixture.ProofActions.stepTheorem"]
                }],
                "report_axioms": true
            }),
        ),
        (
            "lean_lookup",
            json!({
                "kind": "references",
                "name": "LeanRsFixture.ProofActions.stepTheorem",
                "scope": "file",
                "file": "LeanRsFixture/ProofActions.lean",
                "limit": 10
            }),
        ),
    ]
}

fn envelope_status(response: &Value) -> String {
    if semantic_error_code(response).is_some() {
        return semantic_error_code(response).unwrap_or_default();
    }
    for path in ["/result/structuredContent/data", "/result/result/data"] {
        if response.pointer(path).is_some_and(|value| !value.is_null()) {
            return "ok".to_owned();
        }
    }
    "ok".to_owned()
}

fn runtime_error_reason(response: &Value) -> Option<String> {
    for path in [
        "/result/structuredContent/errors/0/details/reason",
        "/result/structuredContent/errors/0/message",
        "/result/result/errors/0/details/reason",
        "/result/result/errors/0/message",
    ] {
        if let Some(reason) = response.pointer(path).and_then(Value::as_str) {
            return Some(reason.to_owned());
        }
    }
    None
}

fn runtime_u64(response: &Value, field: &str) -> Option<u64> {
    response
        .pointer("/result/structuredContent/errors/0/details")
        .and_then(|runtime| runtime.get(field))
        .and_then(Value::as_u64)
}

fn semantic_error_code(response: &Value) -> Option<String> {
    for path in ["/result/structuredContent/errors", "/result/result/errors"] {
        let Some(errors) = response.pointer(path).and_then(Value::as_array) else {
            continue;
        };
        for error in errors {
            if error.get("severity").and_then(Value::as_str) == Some("error")
                && let Some(code) = error.get("code").and_then(Value::as_str)
            {
                return Some(code.to_owned());
            }
        }
    }
    None
}
