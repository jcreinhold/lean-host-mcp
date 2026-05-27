//! Black-box MCP smoke/perf baseline.
//!
//! The test starts the compiled `lean-host-mcp` stdio server, speaks the MCP
//! JSON-RPC protocol over stdin/stdout, and records latency, response bytes,
//! response budget flags, process RSS, and observable project-actor changes.
//! It is intentionally ignored: run it manually when establishing or comparing
//! proof-agent performance baselines.

// Test/harness code favours direct failure over error plumbing; failures should
// tell the developer which measured call stopped producing usable data.
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_mins(3);
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone)]
struct Scenario {
    label: &'static str,
    project_root: PathBuf,
    calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
struct ToolCall {
    label: &'static str,
    tool_name: &'static str,
    category: &'static str,
    arguments: Value,
}

#[derive(Debug, Serialize)]
struct SmokeRecord {
    event: &'static str,
    scenario: String,
    call: String,
    tool: String,
    category: String,
    wall_ms: u128,
    response_bytes: usize,
    over_32k: bool,
    over_64k: bool,
    status: String,
    warnings_count: usize,
    session_changed: bool,
    rss_before_kib: Option<u64>,
    rss_after_kib: Option<u64>,
    infrastructure_event: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolListRecord {
    event: &'static str,
    scenario: String,
    tool_count: usize,
    wall_ms: u128,
    response_bytes: usize,
    over_32k: bool,
    over_64k: bool,
    rss_before_kib: Option<u64>,
    rss_after_kib: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SummaryRecord {
    event: &'static str,
    target_min_bytes: usize,
    target_max_bytes: usize,
    hard_budget_bytes: usize,
    scenarios: usize,
    calls: usize,
    over_32k: usize,
    over_64k: usize,
    errors_or_infra_events: usize,
    slowest_call: Option<String>,
    slowest_wall_ms: u128,
    status_counts: BTreeMap<String, usize>,
}

#[tokio::test]
#[ignore = "manual black-box MCP smoke/perf baseline; use --nocapture to see JSONL"]
async fn black_box_mcp_smoke_perf_baseline() {
    let scenarios = scenarios();
    assert!(
        !scenarios.is_empty(),
        "at least the bundled fixture scenario should be available"
    );

    let mut summary = Summary {
        scenarios: scenarios.len(),
        ..Summary::default()
    };

    for scenario in scenarios {
        run_scenario(&scenario, &mut summary).await;
    }

    let summary_record = summary.into_record();
    eprintln!(
        "smoke_perf summary: scenarios={}, calls={}, over32k={}, over64k={}, errors_or_infra={}, slowest={}ms {:?}",
        summary_record.scenarios,
        summary_record.calls,
        summary_record.over_32k,
        summary_record.over_64k,
        summary_record.errors_or_infra_events,
        summary_record.slowest_wall_ms,
        summary_record.slowest_call,
    );
    println!(
        "{}",
        serde_json::to_string(&summary_record).expect("serialize summary record")
    );
}

async fn run_scenario(scenario: &Scenario, summary: &mut Summary) {
    eprintln!(
        "smoke_perf: starting scenario '{}' ({})",
        scenario.label,
        scenario.project_root.display()
    );
    let mut server = McpServer::start(&scenario.project_root).await;

    let rss_before = server.rss_kib();
    let start = Instant::now();
    let tools_response = server
        .request("tools/list", json!({}))
        .await
        .expect("tools/list response");
    let rss_after = server.rss_kib();
    let tool_count = tools_response
        .json
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let tools_record = ToolListRecord {
        event: "tools_list",
        scenario: scenario.label.to_owned(),
        tool_count,
        wall_ms: start.elapsed().as_millis(),
        response_bytes: tools_response.raw_len,
        over_32k: tools_response.raw_len > support::MODEL_RESPONSE_TARGET_MAX_BYTES,
        over_64k: tools_response.raw_len > support::MODEL_RESPONSE_HARD_BUDGET_BYTES,
        rss_before_kib: rss_before,
        rss_after_kib: rss_after,
    };
    println!(
        "{}",
        serde_json::to_string(&tools_record).expect("serialize tools/list record")
    );

    let mut last_session_id: Option<String> = None;
    for call in &scenario.calls {
        let record = server.call_tool(scenario.label, call, &mut last_session_id).await;
        summary.add(&record);
        println!("{}", serde_json::to_string(&record).expect("serialize smoke record"));
    }

    server.shutdown().await;
}

fn scenarios() -> Vec<Scenario> {
    let mut out = vec![Scenario {
        label: "fixture",
        project_root: fixture_root(),
        calls: fixture_calls(),
    }];

    if let Some(project_root) = env_path("LEAN_HOST_MCP_SMOKE_PROJECT") {
        out.push(Scenario {
            label: "external",
            calls: external_project_calls(),
            project_root,
        });
    }

    if std::env::var("LEAN_HOST_MCP_SMOKE_KANPROOFS").is_ok_and(|value| value == "1") {
        if let Some(project_root) = kanproofs_root() {
            out.push(Scenario {
                label: "kanproofs",
                calls: kanproofs_calls(),
                project_root,
            });
        } else {
            eprintln!("smoke_perf: skipping KanProofs scenario; project root not found");
        }
    }

    out
}

fn fixture_root() -> PathBuf {
    if let Some(root) = env_path("LEAN_HOST_MCP_TEST_FIXTURE") {
        return root;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has workspace parent")
        .parent()
        .expect("workspace has repository parent")
        .join("fixtures/lean")
}

fn kanproofs_root() -> Option<PathBuf> {
    env_path("LEAN_HOST_MCP_SMOKE_KANPROOFS_PROJECT").or_else(|| {
        let default = PathBuf::from("/Users/jcreinhold/Code/kan-proofs");
        default.exists().then_some(default)
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|path| path.exists())
}

fn fixture_calls() -> Vec<ToolCall> {
    vec![
        ToolCall {
            label: "elaborate_nat",
            tool_name: "elaborate",
            category: "term",
            arguments: json!({
                "source": "(Nat.succ 0 : Nat)",
                "imports": ["LeanRsFixture.Handles"]
            }),
        },
        ToolCall {
            label: "kernel_check_theorem",
            tool_name: "kernel_check",
            category: "term",
            arguments: json!({
                "source": "theorem smokePerfKernelCheck : 1 + 1 = 2 := rfl",
                "imports": []
            }),
        },
        ToolCall {
            label: "infer_type_nat_fn",
            tool_name: "infer_type",
            category: "meta",
            arguments: json!({
                "term": "fun (n : Nat) => n + 1",
                "imports": ["LeanRsFixture.Handles"]
            }),
        },
        ToolCall {
            label: "whnf_nat_add",
            tool_name: "whnf",
            category: "meta",
            arguments: json!({
                "term": "(fun n : Nat => n + 0) 4",
                "imports": []
            }),
        },
        ToolCall {
            label: "is_def_eq_nat",
            tool_name: "is_def_eq",
            category: "meta",
            arguments: json!({
                "lhs": "1 + 1",
                "rhs": "2",
                "imports": []
            }),
        },
        ToolCall {
            label: "hover_nat_add_zero",
            tool_name: "hover_by_name",
            category: "declaration",
            arguments: json!({
                "name": "Nat.add_zero",
                "imports": ["LeanRsFixture.Handles"]
            }),
        },
        ToolCall {
            label: "type_of_nat_add_zero",
            tool_name: "type_of_name",
            category: "declaration",
            arguments: json!({
                "name": "Nat.add_zero",
                "imports": ["LeanRsFixture.Handles"]
            }),
        },
        ToolCall {
            label: "search_add_zero",
            tool_name: "search_declarations",
            category: "declaration",
            arguments: json!({
                "query": "add_zero",
                "kind": "theorem",
                "imports": ["LeanRsFixture.Handles"],
                "limit": 20
            }),
        },
        ToolCall {
            label: "project_scan_sorry",
            tool_name: "project_scan",
            category: "source",
            arguments: json!({ "preset": "sorry", "limit": 20 }),
        },
        ToolCall {
            label: "file_diagnostics_source_ranges",
            tool_name: "file_diagnostics",
            category: "position",
            arguments: json!({ "file": "LeanRsFixture/SourceRanges.lean" }),
        },
        ToolCall {
            label: "type_at_known_theorem",
            tool_name: "type_at_position",
            category: "position",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "line": 7,
                "column": 9
            }),
        },
        ToolCall {
            label: "goal_at_trivial",
            tool_name: "goal_at_position",
            category: "position",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "line": 8,
                "column": 3
            }),
        },
        ToolCall {
            label: "references_in_file_known_theorem",
            tool_name: "references_in_file",
            category: "position",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "name": "LeanRsFixture.SourceRanges.knownTheorem"
            }),
        },
        ToolCall {
            label: "references_in_project_known_theorem",
            tool_name: "references_in_project",
            category: "position",
            arguments: json!({
                "name": "LeanRsFixture.SourceRanges.knownTheorem",
                "files": ["LeanRsFixture/SourceRanges.lean"],
                "limit": 20
            }),
        },
    ]
}

fn external_project_calls() -> Vec<ToolCall> {
    let mut calls = vec![
        ToolCall {
            label: "infer_type_no_imports",
            tool_name: "infer_type",
            category: "meta",
            arguments: json!({ "term": "fun (n : Nat) => n + 1", "imports": [] }),
        },
        ToolCall {
            label: "search_add_zero_no_imports",
            tool_name: "search_declarations",
            category: "declaration",
            arguments: json!({ "query": "add_zero", "kind": "theorem", "imports": [], "limit": 20 }),
        },
        ToolCall {
            label: "project_scan_sorry",
            tool_name: "project_scan",
            category: "source",
            arguments: json!({ "preset": "sorry", "limit": 50 }),
        },
    ];
    if let Ok(file) = std::env::var("LEAN_HOST_MCP_SMOKE_FILE") {
        calls.push(ToolCall {
            label: "file_diagnostics_env_file",
            tool_name: "file_diagnostics",
            category: "position",
            arguments: json!({ "file": file }),
        });
    }
    calls
}

fn kanproofs_calls() -> Vec<ToolCall> {
    let file = std::env::var("LEAN_HOST_MCP_SMOKE_KANPROOFS_FILE").unwrap_or_else(|_| {
        "KanProofs/AlgebraicGeometry/Sites/FiniteEtale/Quotient/BaseChange/Restrict.lean".to_owned()
    });
    let line = env_u32("LEAN_HOST_MCP_SMOKE_KANPROOFS_LINE").unwrap_or(127);
    let column = env_u32("LEAN_HOST_MCP_SMOKE_KANPROOFS_COLUMN").unwrap_or(10);

    vec![
        ToolCall {
            label: "file_diagnostics_basechange_restrict",
            tool_name: "file_diagnostics",
            category: "kanproofs-position",
            arguments: json!({ "file": file }),
        },
        ToolCall {
            label: "type_at_basechange_restrict_cursor",
            tool_name: "type_at_position",
            category: "kanproofs-position",
            arguments: json!({
                "file": std::env::var("LEAN_HOST_MCP_SMOKE_KANPROOFS_FILE").unwrap_or_else(|_| {
                    "KanProofs/AlgebraicGeometry/Sites/FiniteEtale/Quotient/BaseChange/Restrict.lean".to_owned()
                }),
                "line": line,
                "column": column
            }),
        },
    ]
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.parse().ok()
}

struct McpServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: tokio::task::JoinHandle<String>,
    next_id: u64,
}

impl McpServer {
    async fn start(project_root: &Path) -> Self {
        let cache_dir = tempfile::tempdir().expect("temp cache dir").keep();
        let config_dir = tempfile::tempdir().expect("temp config dir").keep();
        let mut child = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
            .arg("--lake-root")
            .arg(project_root)
            .env("LEAN_HOST_MCP_CACHE_DIR", cache_dir)
            .env("LEAN_HOST_MCP_CONFIG_DIR", config_dir)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lean-host-mcp");

        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        let stderr = child.stderr.take().expect("server stderr");
        let stderr_task = tokio::spawn(read_stderr(stderr));

        let mut server = Self {
            child,
            stdin,
            stdout,
            stderr_task,
            next_id: 1,
        };
        server.initialize().await;
        server
    }

    async fn initialize(&mut self) {
        let response = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "lean-host-mcp-smoke-perf", "version": "0.0.0" }
                }),
            )
            .await
            .expect("initialize response");
        assert!(
            response.json.get("error").is_none(),
            "initialize must not fail: {:?}",
            response.json
        );
        self.notify("notifications/initialized", json!({})).await;
    }

    async fn call_tool(
        &mut self,
        scenario: &str,
        call: &ToolCall,
        last_session_id: &mut Option<String>,
    ) -> SmokeRecord {
        let rss_before = self.rss_kib();
        let start = Instant::now();
        let response = self
            .request(
                "tools/call",
                json!({
                    "name": call.tool_name,
                    "arguments": call.arguments
                }),
            )
            .await;
        let wall_ms = start.elapsed().as_millis();
        let rss_after = self.rss_kib();

        match response {
            Ok(response) => {
                let session_id = session_id(&response.json);
                let session_changed =
                    matches!((&*last_session_id, &session_id), (Some(prev), Some(next)) if prev != next);
                if let Some(next) = session_id {
                    *last_session_id = Some(next);
                }
                let status = response_status(&response.json);
                let infrastructure_event = infrastructure_event(&response.json, session_changed);
                SmokeRecord {
                    event: "tool_call",
                    scenario: scenario.to_owned(),
                    call: call.label.to_owned(),
                    tool: call.tool_name.to_owned(),
                    category: call.category.to_owned(),
                    wall_ms,
                    response_bytes: response.raw_len,
                    over_32k: response.raw_len > support::MODEL_RESPONSE_TARGET_MAX_BYTES,
                    over_64k: response.raw_len > support::MODEL_RESPONSE_HARD_BUDGET_BYTES,
                    status,
                    warnings_count: warnings_count(&response.json),
                    session_changed,
                    rss_before_kib: rss_before,
                    rss_after_kib: rss_after,
                    infrastructure_event,
                }
            }
            Err(err) => SmokeRecord {
                event: "tool_call",
                scenario: scenario.to_owned(),
                call: call.label.to_owned(),
                tool: call.tool_name.to_owned(),
                category: call.category.to_owned(),
                wall_ms,
                response_bytes: 0,
                over_32k: false,
                over_64k: false,
                status: "transport_error".to_owned(),
                warnings_count: 0,
                session_changed: false,
                rss_before_kib: rss_before,
                rss_after_kib: rss_after,
                infrastructure_event: Some(err),
            },
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<McpResponse, String> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("MCP request id overflow");
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_message(&message).await?;
        self.read_response(id).await
    }

    async fn notify(&mut self, method: &str, params: Value) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.write_message(&message).await.expect("write notification");
    }

    async fn write_message(&mut self, message: &Value) -> Result<(), String> {
        let mut encoded = serde_json::to_vec(message).map_err(|err| err.to_string())?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|err| format!("write request: {err}"))?;
        self.stdin.flush().await.map_err(|err| format!("flush request: {err}"))
    }

    async fn read_response(&mut self, id: u64) -> Result<McpResponse, String> {
        tokio::time::timeout(call_timeout(), async {
            loop {
                let mut line = String::new();
                let bytes = self
                    .stdout
                    .read_line(&mut line)
                    .await
                    .map_err(|err| format!("read response: {err}"))?;
                if bytes == 0 {
                    return Err("server stdout closed".to_owned());
                }
                let json: Value =
                    serde_json::from_str(line.trim_end()).map_err(|err| format!("parse response JSON: {err}"))?;
                if json.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(McpResponse {
                        raw_len: line.len(),
                        json,
                    });
                }
            }
        })
        .await
        .map_err(|_| format!("timed out waiting for response id {id}"))?
    }

    fn rss_kib(&self) -> Option<u64> {
        self.child.id().and_then(process_rss_kib)
    }

    async fn shutdown(mut self) {
        drop(self.stdin);
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_status)) => {}
            Ok(Err(err)) => eprintln!("smoke_perf: server wait failed: {err}"),
            Err(_) => {
                if let Err(err) = self.child.kill().await {
                    eprintln!("smoke_perf: server kill failed: {err}");
                }
            }
        }
        let stderr = self.stderr_task.await.unwrap_or_default();
        if !stderr.trim().is_empty() {
            eprintln!("smoke_perf server stderr:\n{stderr}");
        }
    }
}

struct McpResponse {
    raw_len: usize,
    json: Value,
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
            Err(err) => {
                let _ = writeln!(out, "stderr read error: {err}");
                return out;
            }
        }
    }
}

fn process_rss_kib(pid: u32) -> Option<u64> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

fn call_timeout() -> Duration {
    std::env::var("LEAN_HOST_MCP_SMOKE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(DEFAULT_CALL_TIMEOUT, Duration::from_secs)
}

fn response_status(response: &Value) -> String {
    if let Some(code) = response.pointer("/error/code").and_then(Value::as_i64) {
        return format!("mcp_error:{code}");
    }
    for path in [
        "/result/structuredContent/result/status",
        "/result/structuredContent/status",
        "/result/result/status",
    ] {
        if let Some(status) = response.pointer(path).and_then(Value::as_str) {
            return status.to_owned();
        }
    }
    if let Some(error) = response.pointer("/result/isError").and_then(Value::as_bool)
        && error
    {
        return "tool_error".to_owned();
    }
    "ok".to_owned()
}

fn warnings_count(response: &Value) -> usize {
    response
        .pointer("/result/structuredContent/warnings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn session_id(response: &Value) -> Option<String> {
    response
        .pointer("/result/structuredContent/freshness/session_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn infrastructure_event(response: &Value, session_changed: bool) -> Option<String> {
    if session_changed {
        return Some("session_id_changed".to_owned());
    }
    response
        .pointer("/error/code")
        .and_then(Value::as_i64)
        .map(|code| format!("mcp_error:{code}"))
}

#[derive(Debug, Default)]
struct Summary {
    scenarios: usize,
    calls: usize,
    over_32k: usize,
    over_64k: usize,
    errors_or_infra_events: usize,
    slowest_call: Option<String>,
    slowest_wall_ms: u128,
    status_counts: BTreeMap<String, usize>,
}

impl Summary {
    fn add(&mut self, record: &SmokeRecord) {
        self.calls = self.calls.saturating_add(1);
        self.over_32k = self.over_32k.saturating_add(usize::from(record.over_32k));
        self.over_64k = self.over_64k.saturating_add(usize::from(record.over_64k));
        self.errors_or_infra_events = self
            .errors_or_infra_events
            .saturating_add(usize::from(record.infrastructure_event.is_some()));
        let count = self.status_counts.entry(record.status.clone()).or_default();
        *count = count.saturating_add(1);
        if record.wall_ms > self.slowest_wall_ms {
            self.slowest_wall_ms = record.wall_ms;
            self.slowest_call = Some(format!("{}:{}", record.scenario, record.call));
        }
    }

    fn into_record(self) -> SummaryRecord {
        SummaryRecord {
            event: "summary",
            target_min_bytes: support::MODEL_RESPONSE_TARGET_MIN_BYTES,
            target_max_bytes: support::MODEL_RESPONSE_TARGET_MAX_BYTES,
            hard_budget_bytes: support::MODEL_RESPONSE_HARD_BUDGET_BYTES,
            scenarios: self.scenarios,
            calls: self.calls,
            over_32k: self.over_32k,
            over_64k: self.over_64k,
            errors_or_infra_events: self.errors_or_infra_events,
            slowest_call: self.slowest_call,
            slowest_wall_ms: self.slowest_wall_ms,
            status_counts: self.status_counts,
        }
    }
}
