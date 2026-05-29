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

use std::collections::{BTreeMap, BTreeSet};
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
    worker_cache_status: Option<String>,
    worker_output_bytes: Option<u64>,
    worker_header_import_micros: Option<u64>,
    worker_elaboration_micros: Option<u64>,
    worker_projection_micros: Option<u64>,
    worker_rendering_micros: Option<u64>,
    worker_cache_entry_count: Option<u64>,
    worker_cache_approx_bytes: Option<u64>,
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

#[tokio::test]
#[ignore = "manual black-box MCP burst with tight admission limits"]
async fn black_box_pipelined_admission_pressure_returns_structured_statuses() {
    let scenario = Scenario {
        label: "fixture_admission_pressure",
        project_root: fixture_root(),
        calls: fixture_calls(),
    };
    let mut server = McpServer::start_with_env(
        &scenario.project_root,
        &[
            ("LEAN_HOST_MCP_SEMANTIC_PERMITS", "1"),
            ("LEAN_HOST_MCP_SEMANTIC_WAITERS", "1"),
            ("LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS", "1"),
        ],
    )
    .await;

    let burst = scenario.calls.iter().cycle().take(12).collect::<Vec<_>>();
    let responses = server
        .pipeline_tool_calls(&burst)
        .await
        .expect("pipelined MCP admission-pressure burst should complete");

    for (call, response) in burst.iter().zip(responses.iter()) {
        assert!(
            response.json.get("error").is_none(),
            "pipelined {} returned JSON-RPC error: {:?}",
            call.label,
            response.json
        );
        let status = envelope_status(&response.json);
        assert!(
            matches!(status.as_str(), "ok" | "runtime_unavailable"),
            "pipelined {} returned unexpected envelope status {status}: {:?}",
            call.label,
            response.json
        );
        if status == "runtime_unavailable" {
            let reason = runtime_error_reason(&response.json).unwrap_or_default();
            assert!(
                reason.starts_with("semantic_admission_") || reason == "mailbox_full",
                "runtime pressure should be structured admission/mailbox pressure, got {reason}: {:?}",
                response.json
            );
        }
    }

    let follow_up = server
        .request(
            "tools/call",
            json!({
                "name": "proof_state",
                "arguments": {
                    "file": "LeanRsFixture/SourceRanges.lean",
                    "declaration": "LeanRsFixture.SourceRanges.knownTheorem"
                }
            }),
        )
        .await
        .expect("healthy follow-up proof_state response");
    assert!(
        follow_up.json.get("error").is_none(),
        "follow-up proof_state must not return JSON-RPC error: {:?}",
        follow_up.json
    );
    assert_ne!(response_status(&follow_up.json), "tool_error");
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "manual RSS policy sweep for future threshold tuning"]
async fn rss_threshold_sweep_fixture_sequence_reports_metrics() {
    let scenario = Scenario {
        label: "fixture_rss_sweep",
        project_root: fixture_root(),
        calls: fixture_calls(),
    };
    for threshold in [3_u64 * 1024 * 1024, 5_u64 * 1024 * 1024, 7_u64 * 1024 * 1024] {
        let threshold_s = threshold.to_string();
        let mut server = McpServer::start_with_env(
            &scenario.project_root,
            &[("LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB", threshold_s.as_str())],
        )
        .await;
        let started = Instant::now();
        let mut status_counts = BTreeMap::<String, usize>::new();
        let mut sessions = BTreeSet::<String>::new();
        let mut cache_hits = 0usize;
        let mut peak_rss_kib = server.rss_kib().unwrap_or_default();
        let mut last_session_id = None;

        for call in &scenario.calls {
            let record = server.call_tool("fixture_rss_sweep", call, &mut last_session_id).await;
            *status_counts.entry(record.status.clone()).or_default() += 1;
            if let Some(session) = last_session_id.as_ref() {
                sessions.insert(session.clone());
            }
            if record.worker_cache_status.as_deref() == Some("hit") {
                cache_hits = cache_hits.saturating_add(1);
            }
            if let Some(rss) = record.rss_after_kib {
                peak_rss_kib = peak_rss_kib.max(rss);
            }
        }

        println!(
            "{}",
            serde_json::to_string(&json!({
                "event": "rss_threshold_sweep",
                "threshold_kib": threshold,
                "wall_ms": started.elapsed().as_millis(),
                "session_count": sessions.len(),
                "cache_hits": cache_hits,
                "peak_server_rss_kib": peak_rss_kib,
                "status_counts": status_counts,
            }))
            .expect("serialize RSS sweep record")
        );
        server.shutdown().await;
    }
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
    let tool_names = tools_response
        .json
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    assert!(tool_names.contains("proof_state"), "tools/list must expose proof_state");
    assert!(
        tool_names.contains("inspect_declaration"),
        "tools/list must expose inspect_declaration"
    );
    assert!(
        tool_names.contains("search_for_proof"),
        "tools/list must expose search_for_proof"
    );
    assert!(
        tool_names.contains("try_proof_step"),
        "tools/list must expose try_proof_step"
    );
    assert!(
        tool_names.contains("verify_declaration"),
        "tools/list must expose verify_declaration"
    );
    assert!(
        tool_names.contains("find_references"),
        "tools/list must expose find_references"
    );
    for removed in [
        "lean_query",
        "source_search",
        "mathlib_placement",
        "file_diagnostics",
        "goal_at_position",
        "type_at_position",
        "hover_by_name",
        "type_of_name",
        "search_declarations",
        "project_scan",
        "references_in_file",
        "references_in_project",
        "elaborate",
        "kernel_check",
        "infer_type",
        "whnf",
        "is_def_eq",
    ] {
        assert!(
            !tool_names.contains(removed),
            "tools/list must not expose removed tool {removed}"
        );
    }
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
        let proof_action_file = scenario.project_root.join("LeanRsFixture/ProofActions.lean");
        let before = (call.category == "proof_action" && proof_action_file.exists())
            .then(|| std::fs::read(&proof_action_file).expect("read proof-action fixture before call"));
        let record = server.call_tool(scenario.label, call, &mut last_session_id).await;
        if let Some(before) = before {
            assert_eq!(
                std::fs::read(&proof_action_file).expect("read proof-action fixture after call"),
                before,
                "{} must not mutate fixture source",
                call.label
            );
        }
        summary.add(&record);
        println!("{}", serde_json::to_string(&record).expect("serialize smoke record"));
    }

    if scenario.label == "fixture" {
        let burst = scenario
            .calls
            .iter()
            .filter(|call| {
                matches!(
                    call.label,
                    "proof_state_trivial_warm_repeat"
                        | "inspect_known_theorem"
                        | "verify_known_theorem"
                        | "find_references_file_known_theorem"
                )
            })
            .collect::<Vec<_>>();
        let responses = server
            .pipeline_tool_calls(&burst)
            .await
            .expect("pipelined MCP burst should complete");
        for (call, response) in burst.iter().zip(responses.iter()) {
            assert!(
                response.json.get("error").is_none(),
                "pipelined {} returned JSON-RPC error: {:?}",
                call.label,
                response.json
            );
            let status = response_status(&response.json);
            assert!(
                !status.starts_with("mcp_error:") && status != "tool_error",
                "pipelined {} returned infrastructure status {status}: {:?}",
                call.label,
                response.json
            );
        }
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

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|path| path.exists())
}

fn fixture_calls() -> Vec<ToolCall> {
    vec![
        ToolCall {
            label: "inspect_nat_add_zero",
            tool_name: "inspect_declaration",
            category: "declaration",
            arguments: json!({
                "name": "Nat.add_zero",
                "imports": ["LeanRsFixture.Handles"]
            }),
        },
        ToolCall {
            label: "inspect_known_theorem",
            tool_name: "inspect_declaration",
            category: "declaration",
            arguments: json!({
                "name": "LeanRsFixture.SourceRanges.knownTheorem",
                "file": "LeanRsFixture/SourceRanges.lean",
            }),
        },
        ToolCall {
            label: "inspect_large_statement_truncated",
            tool_name: "inspect_declaration",
            category: "declaration",
            arguments: json!({
                "name": "Lean.Meta.forallTelescopeReducing",
                "imports": ["Lean"],
                "max_field_bytes": 256
            }),
        },
        ToolCall {
            label: "try_proof_step_trivial",
            tool_name: "try_proof_step",
            category: "proof_action",
            arguments: json!({
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.stepTheorem",
                "snippet": "trivial"
            }),
        },
        ToolCall {
            label: "try_proof_step_bad",
            tool_name: "try_proof_step",
            category: "proof_action",
            arguments: json!({
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.stepTheorem",
                "snippet": "exact missingIdentifier"
            }),
        },
        ToolCall {
            label: "try_proof_step_many",
            tool_name: "try_proof_step",
            category: "proof_action",
            arguments: json!({
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.stepTheorem",
                "snippets": [
                    "exact missingIdentifier",
                    "trivial",
                    "exact missingIdentifier",
                    "exact missingIdentifier",
                    "exact missingIdentifier",
                    "exact missingIdentifier",
                    "exact missingIdentifier",
                    "exact missingIdentifier",
                    "exact missingIdentifier"
                ]
            }),
        },
        ToolCall {
            label: "verify_known_theorem",
            tool_name: "verify_declaration",
            category: "proof_action",
            arguments: json!({
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.stepTheorem",
                "report_axioms": true
            }),
        },
        ToolCall {
            label: "verify_sorry_theorem",
            tool_name: "verify_declaration",
            category: "proof_action",
            arguments: json!({
                "file": "LeanRsFixture/ProofActions.lean",
                "declaration": "LeanRsFixture.ProofActions.sorryTheorem"
            }),
        },
        ToolCall {
            label: "proof_state_trivial_cold",
            tool_name: "proof_state",
            category: "position",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "declaration": "LeanRsFixture.SourceRanges.knownTheorem"
            }),
        },
        ToolCall {
            label: "proof_state_trivial_warm_repeat",
            tool_name: "proof_state",
            category: "position",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "declaration": "LeanRsFixture.SourceRanges.knownTheorem"
            }),
        },
        ToolCall {
            label: "search_for_proof_trivial_declaration",
            tool_name: "search_for_proof",
            category: "proof_search",
            arguments: json!({
                "file": "LeanRsFixture/SourceRanges.lean",
                "declaration": "LeanRsFixture.SourceRanges.knownTheorem",
                "limit": 10
            }),
        },
        ToolCall {
            label: "search_for_proof_explicit_true",
            tool_name: "search_for_proof",
            category: "proof_search",
            arguments: json!({
                "goal": "⊢ True",
                "imports": ["LeanRsFixture.SourceRanges"],
                "mode": "exact",
                "limit": 10
            }),
        },
        ToolCall {
            label: "find_references_file_known_theorem",
            tool_name: "find_references",
            category: "position",
            arguments: json!({
                "scope": "file",
                "file": "LeanRsFixture/SourceRanges.lean",
                "name": "LeanRsFixture.SourceRanges.knownTheorem"
            }),
        },
        ToolCall {
            label: "find_references_project_known_theorem",
            tool_name: "find_references",
            category: "position",
            arguments: json!({
                "scope": "project",
                "name": "LeanRsFixture.SourceRanges.knownTheorem",
                "files": ["LeanRsFixture/SourceRanges.lean"],
                "limit": 20
            }),
        },
    ]
}

fn external_project_calls() -> Vec<ToolCall> {
    let mut calls = vec![ToolCall {
        label: "inspect_nat_add_zero_no_imports",
        tool_name: "inspect_declaration",
        category: "declaration",
        arguments: json!({ "name": "Nat.add_zero", "imports": [] }),
    }];
    if let (Ok(file), Ok(declaration)) = (
        std::env::var("LEAN_HOST_MCP_SMOKE_FILE"),
        std::env::var("LEAN_HOST_MCP_SMOKE_DECLARATION"),
    ) {
        calls.push(ToolCall {
            label: "proof_state_env_declaration",
            tool_name: "proof_state",
            category: "position",
            arguments: json!({
                "file": file,
                "declaration": declaration
            }),
        });
    }
    calls
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
        Self::start_with_env(project_root, &[]).await
    }

    async fn start_with_env(project_root: &Path, envs: &[(&str, &str)]) -> Self {
        let config_dir = tempfile::tempdir().expect("temp config dir").keep();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        command
            .arg("--lake-root")
            .arg(project_root)
            .env("LEAN_HOST_MCP_CONFIG_DIR", config_dir)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn lean-host-mcp");

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
                    worker_cache_status: query_fact_str(&response.json, "cache_status"),
                    worker_output_bytes: query_fact_u64(&response.json, "output_bytes"),
                    worker_header_import_micros: query_timing_u64(&response.json, "header_import_micros"),
                    worker_elaboration_micros: query_timing_u64(&response.json, "elaboration_micros"),
                    worker_projection_micros: query_timing_u64(&response.json, "projection_micros"),
                    worker_rendering_micros: query_timing_u64(&response.json, "rendering_micros"),
                    worker_cache_entry_count: query_fact_u64(&response.json, "cache_entry_count"),
                    worker_cache_approx_bytes: query_fact_u64(&response.json, "cache_approx_bytes"),
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
                worker_cache_status: None,
                worker_output_bytes: None,
                worker_header_import_micros: None,
                worker_elaboration_micros: None,
                worker_projection_micros: None,
                worker_rendering_micros: None,
                worker_cache_entry_count: None,
                worker_cache_approx_bytes: None,
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

    async fn pipeline_tool_calls(&mut self, calls: &[&ToolCall]) -> Result<Vec<McpResponse>, String> {
        let mut ids = Vec::with_capacity(calls.len());
        for call in calls {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).expect("MCP request id overflow");
            ids.push(id);
            let message = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": call.tool_name,
                    "arguments": call.arguments
                }
            });
            self.write_message(&message).await?;
        }

        let mut pending = ids.iter().copied().collect::<BTreeSet<_>>();
        let mut by_id = BTreeMap::new();
        tokio::time::timeout(call_timeout(), async {
            while !pending.is_empty() {
                let response = self.read_any_response().await?;
                let Some(id) = response.json.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if pending.remove(&id) {
                    by_id.insert(id, response);
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|_| "timed out waiting for pipelined responses".to_owned())??;

        ids.into_iter()
            .map(|id| {
                by_id
                    .remove(&id)
                    .ok_or_else(|| format!("missing pipelined response id {id}"))
            })
            .collect()
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
                let response = self.read_any_response().await?;
                let json = &response.json;
                if json.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(response);
                }
            }
        })
        .await
        .map_err(|_| format!("timed out waiting for response id {id}"))?
    }

    async fn read_any_response(&mut self) -> Result<McpResponse, String> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|err| format!("read response: {err}"))?;
        if bytes == 0 {
            return Err("server stdout closed".to_owned());
        }
        let json: Value = serde_json::from_str(line.trim_end()).map_err(|err| format!("parse response JSON: {err}"))?;
        Ok(McpResponse {
            raw_len: line.len(),
            json,
        })
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

fn envelope_status(response: &Value) -> String {
    for path in ["/result/structuredContent/status", "/result/result/status"] {
        if let Some(status) = response.pointer(path).and_then(Value::as_str) {
            return status.to_owned();
        }
    }
    response_status(response)
}

fn runtime_error_reason(response: &Value) -> Option<String> {
    for path in [
        "/result/structuredContent/runtime_error/reason",
        "/result/structuredContent/result/runtime_error/reason",
        "/result/result/runtime_error/reason",
    ] {
        if let Some(reason) = response.pointer(path).and_then(Value::as_str) {
            return Some(reason.to_owned());
        }
    }
    None
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

fn query_facts(response: &Value) -> Option<&Value> {
    response.pointer("/result/structuredContent/result/query_facts")
}

fn query_fact_str(response: &Value, field: &str) -> Option<String> {
    query_facts(response)?
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn query_fact_u64(response: &Value, field: &str) -> Option<u64> {
    query_facts(response)?.get(field).and_then(Value::as_u64)
}

fn query_timing_u64(response: &Value, field: &str) -> Option<u64> {
    query_facts(response)?
        .get("timings")?
        .get(field)
        .and_then(Value::as_u64)
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
