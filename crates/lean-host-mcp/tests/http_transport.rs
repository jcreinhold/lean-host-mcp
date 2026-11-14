//! Black-box Streamable HTTP transport tests.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStderr, Command};

const ACCEPT_BOTH: &str = "application/json, text/event-stream";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const EXPECTED_TOOLS: [&str; 7] = [
    "lean_context",
    "lean_lookup",
    "lean_request",
    "lean_runtime",
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
        Some(4),
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
async fn native_http_rejects_every_browser_origin_and_foreign_host() {
    let server = HttpMcpServer::start(&[]).await;
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {},
            "clientInfo": { "name": "origin-test", "version": "1" }
        }
    });
    // GET/DELETE and malformed bodies must not bypass origin validation either.
    for method in [reqwest::Method::POST, reqwest::Method::GET, reqwest::Method::DELETE] {
        for origins in [
            vec!["https://attacker.example"],
            vec!["null"],
            vec![""],
            vec!["not an origin"],
            vec!["http://127.0.0.1"],
            vec!["https://attacker.example", "null"],
            vec!["null", "https://attacker.example"],
        ] {
            let mut headers = HeaderMap::new();
            for origin in &origins {
                headers.append(reqwest::header::ORIGIN, origin.parse().unwrap());
            }
            let response = server
                .client
                .request(method.clone(), &server.url)
                .header("Accept", ACCEPT_BOTH)
                .header("Content-Type", "application/json")
                .headers(headers)
                .body(initialize.to_string())
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                reqwest::StatusCode::FORBIDDEN,
                "{method} {origins:?}"
            );
        }
    }
    let response = server
        .client
        .post(&server.url)
        .header("Accept", ACCEPT_BOTH)
        .header("Content-Type", "application/json")
        .header("Host", "attacker.example")
        .body(initialize.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    // An ordinary native client has no Origin and still negotiates normally.
    let mut session = server.new_session().await;
    assert!(
        session
            .request("tools/list", json!({}))
            .await
            .unwrap()
            .http_status
            .is_success()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn modern_stateless_discovery_calls_and_protocol_guards() {
    const VERSION: &str = "2026-07-28";
    let server = HttpMcpServer::start(&[]).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientInfo": { "name": "modern-test", "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let discover = server
        .modern_request(
            "server/discover",
            None,
            VERSION,
            json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}),
        )
        .await;
    assert!(discover.http_status.is_success(), "{discover:?}");
    assert!(discover.session_id.is_none());
    assert_eq!(
        discover.json.pointer("/result/resultType").and_then(Value::as_str),
        Some("complete")
    );
    assert!(
        discover
            .json
            .pointer("/result/supportedVersions")
            .and_then(Value::as_array)
            .unwrap()
            .contains(&json!(VERSION))
    );
    assert!(discover.json.pointer("/result/capabilities/tools").is_some());

    let listed = server
        .modern_request(
            "tools/list",
            None,
            VERSION,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":meta}}),
        )
        .await;
    assert!(listed.http_status.is_success(), "{listed:?}");
    assert!(listed.session_id.is_none());
    assert_eq!(tool_names(&listed.json), expected_tools());

    let call = server
        .modern_request(
            "tools/call",
            Some("lean_context"),
            VERSION,
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"lean_context","arguments":{"kind":"bogus"},"_meta":meta}}),
        )
        .await;
    assert!(call.http_status.is_success(), "{call:?}");
    assert!(call.session_id.is_none());
    assert_eq!(
        call.json.pointer("/result/isError").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(semantic_error_code(&call.json).as_deref(), Some("invalid_kind"));

    let mismatch = server
        .modern_request(
            "tools/list",
            None,
            VERSION,
            json!({"jsonrpc":"2.0","id":4,"method":"server/discover","params":{"_meta":meta}}),
        )
        .await;
    assert_eq!(mismatch.http_status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        mismatch.json.pointer("/error/code").and_then(Value::as_i64),
        Some(-32020)
    );
    let mut unsupported_meta = meta.clone();
    unsupported_meta["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let unsupported = server
        .modern_request(
            "server/discover",
            None,
            "2099-01-01",
            json!({"jsonrpc":"2.0","id":5,"method":"server/discover","params":{"_meta":unsupported_meta}}),
        )
        .await;
    assert_eq!(unsupported.http_status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        unsupported.json.pointer("/error/code").and_then(Value::as_i64),
        Some(-32022)
    );
    let task = server
        .modern_request(
            "tasks/get",
            Some("unknown-task"),
            VERSION,
            json!({"jsonrpc":"2.0","id":6,"method":"tasks/get","params":{"taskId":"unknown-task","_meta":meta}}),
        )
        .await;
    assert_eq!(task.json.pointer("/error/code").and_then(Value::as_i64), Some(-32021));
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
    assert_eq!(
        response.json.pointer("/result/isError").and_then(Value::as_bool),
        Some(true)
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
async fn streamable_http_concurrent_sessions_surface_structured_pressure() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    // One project in the pool, so all six sessions contend for one worker's
    // mailbox — the server's only admission mechanism.
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_str())],
        &[
            ("LEAN_HOST_MCP_MAX_PROJECTS", "1"),
            ("LEAN_HOST_MCP_TELEMETRY_VERBOSITY", "full"),
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
                saw_wait |= runtime_u64(&response.json, "queue_wait_millis").unwrap_or_default() > 0;
            }
            "runtime_unavailable" => {
                let reason = runtime_error_reason(&response.json).unwrap_or_default();
                assert!(
                    matches!(
                        reason.as_str(),
                        "mailbox_admission_timeout" | "mailbox_closed" | "project_admission_timeout"
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
    // The contract under test is that runtime pressure is never an HTTP or
    // JSON-RPC error — asserted per response above. Six sessions against a
    // 16-deep mailbox need not shed, so neither `saw_pressure` nor `saw_wait`
    // is required; they are reported below for the operator record.
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

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_five_clients_serialize_source_consistent_results() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    let expected =
        sha256_hex(&std::fs::read(root.join("LeanRsFixture/ProofActions.lean")).expect("read fixture source"));
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_str())],
        &[
            ("LEAN_HOST_MCP_MAX_PROJECTS", "1"),
            ("LEAN_HOST_MCP_TELEMETRY_VERBOSITY", "full"),
        ],
    )
    .await;
    let mut tasks = Vec::new();
    for _ in 0..5 {
        let mut session = server.new_session().await;
        tasks.push(tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({
                        "name": "lean_status",
                        "arguments": {
                            "kind": "file_diagnostics",
                            "file": "LeanRsFixture/ProofActions.lean"
                        }
                    }),
                )
                .await
        }));
    }

    let mut waits = Vec::new();
    for task in tasks {
        let response = task.await.expect("HTTP task join").expect("HTTP tool response");
        assert!(response.http_status.is_success(), "response: {response:?}");
        assert!(response.json.get("error").is_none(), "response: {:?}", response.json);
        assert_eq!(
            source_content_digest(&response.json, "LeanRsFixture/ProofActions.lean"),
            Some(expected.as_str()),
            "every client must receive trust for the same source bytes: {:?}",
            response.json
        );
        waits.push(success_runtime_u64(&response.json, "queue_wait_millis").unwrap_or_default());
    }
    assert!(
        waits.iter().any(|wait| *wait > 0),
        "five same-project clients must observe the single actor queue: {waits:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn streamable_http_task_results_survive_new_client() {
    let root = tempfile::tempdir().expect("metadata project");
    std::fs::write(root.path().join("lakefile.lean"), "package lifecycle\n").unwrap();
    std::fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
    std::fs::write(root.path().join("lake-manifest.json"), "{}\n").unwrap();
    let root_s = root.path().to_string_lossy();
    let server = HttpMcpServer::start(&[("--lake-root", root_s.as_ref())]).await;
    let mut legacy = server.new_session().await;
    let arguments = json!({"name": "lean_verify", "arguments": {"targets": []}});
    let synchronous = legacy.request("tools/call", arguments.clone()).await.unwrap();
    assert!(synchronous.json.pointer("/result/taskId").is_none());
    assert!(synchronous.json.pointer("/result/structuredContent").is_some());

    let mut original = TaskHttpClient::new(&server.url);
    let accepted = original.request("tools/call", arguments).await;
    let task_id = task_id(&accepted).to_owned();
    assert_eq!(
        accepted.pointer("/result/resultType").and_then(Value::as_str),
        Some("task")
    );
    let immediate = original.request("tasks/get", json!({"taskId": task_id})).await;
    assert_eq!(
        immediate.pointer("/result/taskId").and_then(Value::as_str),
        Some(task_id.as_str())
    );
    drop(original);
    // A different HTTP client, with no session id or initialized connection,
    // must retrieve the same server-owned task and its unchanged tool payload.
    let mut reconnected = TaskHttpClient::new(&server.url);
    let terminal = reconnected.terminal(&task_id, Duration::from_secs(5)).await;
    assert_eq!(
        terminal.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    let mut retained = terminal.pointer("/result/result").unwrap().clone();
    let mut synchronous = synchronous.json.get("result").unwrap().clone();
    // The current protocol adds its result discriminator; tool content and
    // semantic metadata must otherwise be identical to the legacy response.
    retained.as_object_mut().unwrap().remove("resultType");
    synchronous.as_object_mut().unwrap().remove("resultType");
    assert_eq!(retained, synchronous);
    let status = reconnected
        .request(
            "tools/call",
            json!({"name": "lean_status", "arguments": {"kind": "project"}}),
        )
        .await;
    assert!(
        status.pointer("/result/taskId").is_none(),
        "cheap metadata remains synchronous: {status}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn streamable_http_plain_request_bridge_retains_results_and_deduplicates_retries() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lakefile.lean"), "package retained\n").unwrap();
    std::fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
    std::fs::write(root.path().join("lake-manifest.json"), "{}\n").unwrap();
    let root_s = root.path().to_string_lossy();
    let server = HttpMcpServer::start(&[("--lake-root", root_s.as_ref())]).await;
    let mut original = server.new_session().await;
    let submission = json!({"name": "lean_request", "arguments": {
        "action": "submit", "request_id": "lost-receipt", "tool": "lean_verify", "arguments": {"targets": []}
    }});
    let accepted = original.request("tools/call", submission.clone()).await.unwrap();
    assert_eq!(
        accepted.json.pointer("/result/structuredContent/taskId"),
        Some(&json!("lost-receipt"))
    );
    let binding = accepted
        .json
        .pointer("/result/structuredContent/request")
        .unwrap()
        .clone();
    assert_eq!(binding["tool"], "lean_verify");
    assert_eq!(binding["project"], Value::Null);
    assert_eq!(
        binding["submission_sha256"],
        "27f1aae6bb5bec5f6b58dc30c3bdda13d76fefd07d5e1d1a72f5116c651f5b23"
    );
    let mut status_client = TaskHttpClient::new(&server.url);
    let status = status_client.control(json!({"action":"status"})).await;
    assert_eq!(binding["server_instance_id"], status["pool"]["instance_id"]);
    assert_eq!(
        status["capabilities"],
        json!(["retained_request_binding_v1", "retained_request_lifecycle_v2"])
    );
    // No Tasks negotiation, and the caller may disappear while work continues.
    drop(original);
    let mut reconnected = server.new_session().await;
    let retry = reconnected.request("tools/call", submission).await.unwrap();
    assert_eq!(
        retry.json.pointer("/result/structuredContent/taskId"),
        Some(&json!("lost-receipt"))
    );
    let retained = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = reconnected
                .request(
                    "tools/call",
                    json!({"name": "lean_request", "arguments": {
                        "action": "get", "request_id": "lost-receipt"
                    }}),
                )
                .await
                .unwrap();
            let task = response.json.pointer("/result/structuredContent").unwrap().clone();
            if task["status"] != "working" {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(retained["status"], "completed", "{retained}");
    assert_eq!(retained["request"], binding);
    assert!(
        retained.pointer("/result/structuredContent/errors").is_some(),
        "original invalid-target result retained: {retained}"
    );
    let conflicting = reconnected.request("tools/call", json!({"name": "lean_request", "arguments": {
        "action": "submit", "request_id": "lost-receipt", "tool": "lean_status", "arguments": {"kind": "project"}
    }})).await.unwrap();
    assert!(
        conflicting.json.get("error").is_some(),
        "changed inputs must not rerun: {conflicting:?}"
    );
    let cancelled = reconnected
        .request(
            "tools/call",
            json!({"name": "lean_request", "arguments": {
                "action": "cancel", "request_id": "lost-receipt"
            }}),
        )
        .await
        .unwrap();
    assert_eq!(
        cancelled.json.pointer("/result/structuredContent"),
        Some(&retained),
        "cancellation cannot erase a finished result"
    );
    let mut task_client = TaskHttpClient::new(&server.url);
    let get = task_client
        .request(
            "tools/call",
            json!({"name": "lean_request", "arguments": {
                "action": "get", "request_id": "lost-receipt"
            }}),
        )
        .await;
    assert_eq!(
        get.pointer("/result/structuredContent"),
        Some(&retained),
        "bridge remains synchronous for task-aware clients"
    );
    let unknown = reconnected
        .request(
            "tools/call",
            json!({"name": "lean_request", "arguments": {
                "action": "get", "request_id": "missing"
            }}),
        )
        .await
        .unwrap();
    assert!(unknown.json.get("error").is_some());
    server.shutdown().await;
}

#[tokio::test]
async fn durable_v2_result_survives_binary_restart_and_exact_acknowledgment() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lakefile.lean"), "package retained\n").unwrap();
    std::fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
    std::fs::write(root.path().join("lake-manifest.json"), "{}\n").unwrap();
    let journal = tempfile::tempdir().unwrap();
    let args = [
        ("--lake-root", root.path().to_str().unwrap()),
        ("--request-journal", journal.path().to_str().unwrap()),
    ];
    let server = HttpMcpServer::start(&args).await;
    let mut control = TaskHttpClient::new(&server.url);
    let status = control.control(json!({"action":"status"})).await;
    assert_eq!(status["request_registry"]["durable"], true);
    assert_eq!(status["pool"]["occupied_slots"], 0);
    let epoch = status["request_registry"]["request_epoch"].clone();
    let submission = json!({"name":"lean_request","arguments":{
        "action":"submit","request_id":"durable-receipt","request_epoch":epoch,
        "tool":"lean_status","arguments":{"kind":"project"}
    }});
    let mut client = server.new_session().await;
    client.request("tools/call", submission.clone()).await.unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = client
                .request(
                    "tools/call",
                    json!({"name":"lean_request","arguments":{
                        "action":"get","request_id":"durable-receipt","request_epoch":epoch
                    }}),
                )
                .await
                .unwrap();
            let record = response.json["result"]["structuredContent"].clone();
            if record["status"] != "working" {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["execution"]["disposition"], "stopped");
    assert_eq!(completed["result_available"], true);
    // Kill only this test-owned, worker-free broker. The terminal snapshot must
    // already be durable before the reply can be observed.
    server.shutdown().await;
    let restarted = HttpMcpServer::start(&args).await;
    let mut control = TaskHttpClient::new(&restarted.url);
    let new_status = control.control(json!({"action":"status"})).await;
    assert_ne!(new_status["request_registry"]["request_epoch"], epoch);
    assert_ne!(
        new_status["pool"]["instance_id"],
        completed["request"]["server_instance_id"]
    );
    assert_eq!(new_status["pool"]["occupied_slots"], 0);
    let mut client = restarted.new_session().await;
    let retry = client.request("tools/call", submission).await.unwrap();
    assert_eq!(retry.json["result"]["structuredContent"], completed);
    let ack = json!({"name":"lean_request","arguments":{
        "action":"acknowledge","request_id":"durable-receipt","request_epoch":epoch,
        "submission_sha256":completed["request"]["submission_sha256"]
    }});
    for _ in 0..2 {
        let response = client.request("tools/call", ack.clone()).await.unwrap();
        let record = &response.json["result"]["structuredContent"];
        assert_eq!(record["acknowledged"], true);
        assert_eq!(record["result_available"], false);
        assert_eq!(record["execution"]["disposition"], "stopped");
        assert!(record.get("result").is_none());
    }
    let stale = client
        .request(
            "tools/call",
            json!({"name":"lean_request","arguments":{
                "action":"submit","request_id":"forgotten-old-id","request_epoch":epoch,
                "tool":"lean_status","arguments":{"kind":"project"}
            }}),
        )
        .await
        .unwrap();
    assert_eq!(stale.json["error"]["data"]["execution"]["disposition"], "unknown");
    restarted.shutdown().await;
}

#[tokio::test]
async fn streamable_http_status_reports_resolved_settings_without_opening_worker() {
    let root = tempfile::tempdir().unwrap();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root.path().to_str().unwrap())],
        &[
            ("LEAN_HOST_MCP_MAX_PROJECTS", "2"),
            ("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS", "91"),
            ("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS", "12345"),
            ("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY", "9"),
            ("LEAN_HOST_MCP_WORKER_RESTART_LIMIT", "7"),
            ("LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS", "83"),
            ("LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB", "4096"),
            ("LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY", "6"),
        ],
    )
    .await;
    let mut client = TaskHttpClient::new(&server.url);
    let status = client.control(json!({"action":"status"})).await;
    assert_eq!(status["status"], "ok");
    assert_eq!(status["build"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        status["build"]["revision"],
        json!(option_env!("LEAN_HOST_MCP_BUILD_REVISION"))
    );
    assert_eq!(
        status["build"]["dirty"],
        json!(option_env!("LEAN_HOST_MCP_BUILD_DIRTY").map(|value| value == "true"))
    );
    assert_eq!(status["pool"]["occupied_slots"], 0);
    assert_eq!(status["pool"]["workers_stopped"], true);
    assert_eq!(
        status["effective_settings"]["broker"],
        json!({"max_projects":2,"idle_timeout_secs":91})
    );
    let runtime = &status["effective_settings"]["runtime"];
    assert_eq!(runtime["request_timeout_millis"], 12_345);
    assert_eq!(runtime["project_mailbox_capacity"], 9);
    assert_eq!(runtime["worker_restart_limit"], 7);
    assert_eq!(runtime["worker_restart_window_secs"], 83);
    assert_eq!(runtime["worker_import_residue_budget_bytes"], 4_294_967_296_u64);
    assert_eq!(runtime["worker_import_residue_soft_bytes"], 2_576_980_320_u64);
    assert_eq!(runtime["worker_session_pool_capacity"], 6);
    assert_eq!(status["effective_settings"]["server"]["response_carrier"], "both");
    assert_eq!(
        status["request_registry"],
        json!({"active":0,"retained_terminal":0,"capacity":128,"terminal_retention_millis":86_400_000,
            "request_epoch":status["request_registry"]["request_epoch"],"durable":false,"journal_healthy":true,"max_record_bytes":4*1024*1024})
    );
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_retained_requests_wait_past_full_mailbox_and_cancel() {
    let scratch = tempfile::tempdir().unwrap();
    let running = scratch.path().join("running");
    let finished = scratch.path().join("finished");
    let cancelled = scratch.path().join("cancelled-must-not-run");
    let first = scratch.path().join("first");
    let last = scratch.path().join("last");
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[
            ("LEAN_HOST_MCP_MAX_PROJECTS", "1"),
            ("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY", "1"),
        ],
    )
    .await;
    let mut client = TaskHttpClient::new(&server.url);
    let blocker = client
        .command(&sentinel_command(&running, Some((&finished, 4_000))))
        .await;
    wait_for_sentinel(&running, Duration::from_secs(30)).await;
    let first_id = client.command(&sentinel_command(&first, None)).await;
    let cancel_id = client.command(&sentinel_command(&cancelled, None)).await;
    let last_id = client.command(&sentinel_command(&last, None)).await;
    assert!(!finished.exists());
    client.request("tasks/cancel", json!({"taskId": cancel_id})).await;
    let cancel_result = client.terminal(&cancel_id, Duration::from_secs(10)).await;
    assert_eq!(cancel_result.pointer("/result/status"), Some(&json!("completed")));
    drop(client);
    let mut reconnected = TaskHttpClient::new(&server.url);
    for id in [blocker, first_id, last_id] {
        let result = reconnected.terminal(&id, Duration::from_secs(15)).await;
        assert_eq!(result.pointer("/result/status"), Some(&json!("completed")), "{result}");
        assert_eq!(
            result.pointer("/result/result/isError"),
            Some(&json!(false)),
            "{result}"
        );
        assert!(
            result.pointer("/result/result/structuredContent/data/output").is_some(),
            "{result}"
        );
    }
    assert!(finished.exists());
    assert!(first.exists());
    assert!(last.exists());
    assert!(!cancelled.exists());
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_queued_task_cancellation_skips_lean() {
    let scratch = tempfile::tempdir().expect("test-owned sentinels");
    let running = scratch.path().join("running");
    let finished = scratch.path().join("finished");
    let forbidden = scratch.path().join("cancelled-job-started");
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[("LEAN_HOST_MCP_MAX_PROJECTS", "1")],
    )
    .await;
    let mut client = TaskHttpClient::new(&server.url);
    let blocker = client
        .command(&sentinel_command(&running, Some((&finished, 4_000))))
        .await;
    wait_for_sentinel(&running, Duration::from_secs(30)).await;
    let queued = client.command(&sentinel_command(&forbidden, None)).await;
    assert!(!finished.exists(), "second request must be accepted while Lean is busy");
    let ack = client.request("tasks/cancel", json!({"taskId": queued})).await;
    assert_eq!(
        ack.pointer("/result/resultType").and_then(Value::as_str),
        Some("complete")
    );
    let cancelled = client.terminal(&queued, Duration::from_secs(10)).await;
    assert_eq!(
        cancelled.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    let blocker = client.terminal(&blocker, Duration::from_secs(10)).await;
    assert_eq!(
        blocker.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        finished.exists(),
        "blocking Lean command must finish normally: {blocker}"
    );
    assert!(!forbidden.exists(), "cancelled waiting command must never enter Lean");
    let follow_up = scratch.path().join("healthy-follow-up");
    let healthy = client.command(&sentinel_command(&follow_up, None)).await;
    let result = client.terminal(&healthy, Duration::from_secs(10)).await;
    assert_eq!(
        result.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        follow_up.exists(),
        "same worker must remain usable after queued cancellation: {result}"
    );
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_running_task_cancellation_stops_worker_and_drains() {
    let scratch = tempfile::tempdir().expect("test-owned sentinels");
    let started = scratch.path().join("started");
    let forbidden = scratch.path().join("ran-after-cancellation");
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[("LEAN_HOST_MCP_MAX_PROJECTS", "1")],
    )
    .await;
    let mut client = TaskHttpClient::new(&server.url);
    let task = client
        .command(&sentinel_command(&started, Some((&forbidden, 4_000))))
        .await;
    wait_for_sentinel(&started, Duration::from_secs(30)).await;
    assert!(
        !forbidden.exists(),
        "cancellation must be requested before Lean finishes"
    );
    client.request("tasks/cancel", json!({"taskId": task})).await;
    // The cancellation must settle before the uncancelled computation's sleep
    // would end; merely forgetting its HTTP request cannot pass this check.
    let cancelled = client.terminal(&task, Duration::from_secs(2)).await;
    assert_eq!(
        cancelled.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    let healthy = scratch.path().join("healthy-follow-up");
    let follow_up = client.command(&sentinel_command(&healthy, None)).await;
    let response = client.terminal(&follow_up, Duration::from_secs(10)).await;
    assert_eq!(
        response.pointer("/result/status").and_then(Value::as_str),
        Some("completed")
    );
    assert!(
        healthy.exists(),
        "worker must accept work after cancellation drains: {response}"
    );
    // Catch detached Lean execution that survives a prematurely terminal task.
    tokio::time::sleep(Duration::from_millis(4_250)).await;
    assert!(
        !forbidden.exists(),
        "cancelled worker computation continued in the background"
    );
    server.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_cancelled_startup_preserves_coalesced_worker_at_capacity() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = tempfile::Builder::new()
        .prefix("lean-startup-'wrapper-")
        .tempdir()
        .expect("test-owned delayed worker");
    let worker = support::built_workers_dir(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .expect("build the real worker alongside the parent before running this test")
        .join("lean-host-mcp-worker")
        .canonicalize()
        .expect("exact real worker path");
    let starts = scratch.path().join("worker-starts");
    let release = scratch.path().join("release-startup");
    let wrapper = scratch.path().join("lean-host-mcp-worker");
    // Delay the actual worker handshake, not a Lean command. The first caller
    // must therefore still own the same-project opening lock when cancelled.
    // Bound the wrapper wait so an assertion failure cannot leave a child stuck.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >> {}\nremaining=500\nwhile [ ! -f {} ]; do\n  remaining=$((remaining - 1))\n  [ \"$remaining\" -gt 0 ] || exit 75\n  sleep 0.01\ndone\nexec {} \"$@\"\n",
        shell_quote_path(&starts),
        shell_quote_path(&release),
        shell_quote_path(&worker),
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let wrapper_dir = scratch.path().to_str().expect("UTF-8 test path");
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[
            ("LEAN_HOST_MCP_MAX_PROJECTS", "1"),
            ("LEAN_HOST_MCP_WORKERS_DIR", wrapper_dir),
        ],
    )
    .await;
    let mut opener = TaskHttpClient::new(&server.url);
    let forbidden = scratch.path().join("cancelled-opener-entered-lean");
    let opening = opener.command(&sentinel_command(&forbidden, None)).await;
    wait_for_sentinel(&starts, Duration::from_secs(30)).await;
    let mut waiter = TaskHttpClient::new(&server.url);
    let healthy = scratch.path().join("surviving-waiter-entered-lean");
    let waiting = waiter.command(&sentinel_command(&healthy, None)).await;
    let status = waiter.request("tasks/get", json!({"taskId": waiting})).await;
    assert_eq!(
        status.pointer("/result/status").and_then(Value::as_str),
        Some("working")
    );
    assert!(!healthy.exists(), "same-project request must wait for startup");
    opener.request("tasks/cancel", json!({"taskId": opening})).await;
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap().lines().count(),
        1,
        "only one worker may start while the capacity-one startup is held"
    );
    std::fs::write(&release, "continue").unwrap();
    let cancelled = opener.terminal(&opening, Duration::from_secs(30)).await;
    assert_eq!(
        cancelled.pointer("/result/status").and_then(Value::as_str),
        Some("completed"),
        "opening caller must observe its own cancellation: {cancelled}"
    );
    let completed = waiter.terminal(&waiting, Duration::from_secs(30)).await;
    assert_eq!(
        completed.pointer("/result/status").and_then(Value::as_str),
        Some("completed"),
        "surviving caller must reuse the new worker rather than report a busy pool: {completed}"
    );
    assert!(
        healthy.exists(),
        "surviving caller must actually execute Lean: {completed}"
    );
    assert!(
        !forbidden.exists(),
        "cancelled opener must not execute its Lean command"
    );
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap().lines().count(),
        1,
        "startup must use one retained child, with no temporary preflight or replacement child"
    );
    server.shutdown().await;
}

#[cfg(unix)]
fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.to_str().expect("UTF-8 test path").replace('\'', "'\\''"))
}

#[tokio::test]
async fn streamable_http_runtime_lease_is_immediate_and_owned_across_connections() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lake-manifest.json"), "{}\n").unwrap();
    std::fs::write(root.path().join("lakefile.toml"), "name = \"control\"\n").unwrap();
    std::fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
    let root_s = root.path().to_string_lossy();
    let server = HttpMcpServer::start(&[("--lake-root", root_s.as_ref())]).await;
    let mut owner = TaskHttpClient::new(&server.url);
    let acquired = owner.control(json!({"action": "acquire"})).await;
    assert_eq!(acquired["status"], "acquired");
    assert_eq!(acquired["pool"]["workers_stopped"], true);
    let token = acquired["lease_id"].as_str().unwrap().to_owned();
    drop(owner);
    let mut reconnected = TaskHttpClient::new(&server.url);
    let status = reconnected.control(json!({"action": "status"})).await;
    assert_eq!(status["pool"]["admission_paused"], true);
    assert!(status.get("lease_id").is_none());
    assert_eq!(status["pool"]["instance_id"], acquired["pool"]["instance_id"]);
    let invalid = reconnected
        .control(json!({"action": "release", "lease_id": "wrong"}))
        .await;
    assert_eq!(invalid["status"], "invalid_lease");
    let mut legacy = server.new_session().await;
    let denied = legacy.request("tools/call", json!({"name": "lean_trial", "arguments": {"kind": "command", "imports": ["Init"], "commands": "#check Nat"}})).await.unwrap();
    assert_eq!(
        runtime_error_reason(&denied.json).as_deref(),
        Some("worker_pool_leased"),
        "{}",
        denied.json
    );
    assert!(denied.json.pointer("/result/taskId").is_none());
    let metadata = legacy
        .request(
            "tools/call",
            json!({"name": "lean_status", "arguments": {"kind": "project"}}),
        )
        .await
        .unwrap();
    assert!(
        metadata
            .json
            .pointer("/result/structuredContent/data/worker_opened")
            .is_some_and(|opened| opened == false)
    );
    let renewed = reconnected
        .control(json!({"action": "renew", "lease_id": token, "lease_seconds": 60}))
        .await;
    assert_eq!(renewed["status"], "ok");
    assert!(renewed["pool"]["lease_remaining_millis"].as_u64().unwrap() > 30_000);
    let released = reconnected
        .control(json!({"action": "release", "lease_id": token}))
        .await;
    assert_eq!(released["status"], "ok");
    assert_eq!(released["pool"]["admission_paused"], false);
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires built Lean fixture and worker binary"]
async fn streamable_http_runtime_lease_preserves_warm_workers_and_retires_only_idle_work() {
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[("LEAN_HOST_MCP_MAX_PROJECTS", "1")],
    )
    .await;
    let mut client = TaskHttpClient::new(&server.url);
    let warm = client.command("#eval (1 : Nat)").await;
    assert_eq!(
        client.terminal(&warm, Duration::from_secs(30)).await["result"]["status"],
        "completed"
    );
    let acquired = client.acquire_idle(false).await;
    assert_eq!(acquired["pool"]["projects"].as_array().unwrap().len(), 1);
    assert_eq!(acquired["pool"]["occupied_slots"], 1);
    assert_eq!(acquired["retirement_requested"], 0);
    let generation = acquired["pool"]["projects"][0]["runtime"]["worker_generation"].clone();
    let token = acquired["lease_id"].as_str().unwrap().to_owned();
    client.control(json!({"action": "release", "lease_id": token})).await;
    let repeat = client.command("#eval (2 : Nat)").await;
    assert_eq!(
        client.terminal(&repeat, Duration::from_secs(10)).await["result"]["status"],
        "completed"
    );
    let scratch = tempfile::tempdir().unwrap();
    let started = scratch.path().join("started");
    let finished = scratch.path().join("finished");
    let running = client
        .command(&sentinel_command(&started, Some((&finished, 4_000))))
        .await;
    wait_for_sentinel(&started, Duration::from_secs(10)).await;
    let busy = client.control(json!({"action": "acquire", "retire_idle": true})).await;
    assert_eq!(busy["status"], "busy");
    assert!(busy.get("lease_id").is_none());
    assert_eq!(busy["pool"]["projects"][0]["idle"], false);
    assert!(busy["pool"]["projects"][0]["outstanding_jobs"].as_u64().unwrap() >= 1);
    assert_eq!(busy["pool"]["projects"][0]["runtime"]["worker_generation"], generation);
    assert_eq!(
        client.terminal(&running, Duration::from_secs(10)).await["result"]["status"],
        "completed"
    );
    assert!(finished.exists(), "claiming must not cancel existing work");
    let retired = client.acquire_idle(true).await;
    assert_eq!(retired["retirement_requested"], 1);
    assert!(retired["pool"]["projects"].as_array().unwrap().is_empty());
    let token = retired["lease_id"].as_str().unwrap().to_owned();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = client.control(json!({"action": "status"})).await;
            assert_eq!(status["pool"]["admission_paused"], true);
            if status["pool"]["workers_stopped"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("retired child must confirm exit");
    let blocked = client.command("#eval (3 : Nat)").await;
    let blocked = client.terminal(&blocked, Duration::from_secs(10)).await;
    assert_eq!(
        runtime_error_reason(&json!({"result": blocked["result"]["result"]})).as_deref(),
        Some("worker_pool_leased")
    );
    let still_stopped = client.control(json!({"action": "status"})).await;
    assert_eq!(still_stopped["pool"]["workers_stopped"], true);
    client.control(json!({"action": "release", "lease_id": token})).await;
    let healthy = scratch.path().join("healthy");
    let next = client.command(&sentinel_command(&healthy, None)).await;
    assert_eq!(
        client.terminal(&next, Duration::from_secs(10)).await["result"]["status"],
        "completed"
    );
    assert!(healthy.exists());
    server.shutdown().await;
}

struct TaskHttpClient {
    client: reqwest::Client,
    url: String,
    next_id: u64,
}

impl TaskHttpClient {
    async fn control(&mut self, arguments: Value) -> Value {
        let response = self
            .request("tools/call", json!({"name": "lean_runtime", "arguments": arguments}))
            .await;
        assert!(
            response.pointer("/result/taskId").is_none(),
            "control must remain synchronous: {response}"
        );
        response
            .pointer("/result/structuredContent")
            .expect("structured control response")
            .clone()
    }

    async fn acquire_idle(&mut self, retire_idle: bool) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = self
                    .control(json!({"action": "acquire", "retire_idle": retire_idle}))
                    .await;
                if result["status"] == "acquired" {
                    return result;
                }
                assert_eq!(result["status"], "busy");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("finished worker must become claimable")
    }

    fn new(url: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(35))
                .build()
                .unwrap(),
            url: url.to_owned(),
            next_id: 1,
        }
    }

    async fn request(&mut self, method: &str, mut params: Value) -> Value {
        use rmcp::model::{ClientCapabilities, Implementation, ProtocolVersion, RequestMetaObject};
        let meta = RequestMetaObject::with_client_context(
            ProtocolVersion::V_2026_07_28,
            Implementation::new("lean-host-lifecycle-test", "1"),
            ClientCapabilities::builder().enable_tasks().build(),
        );
        params["_meta"] = serde_json::to_value(meta).unwrap();
        let name = if method == "tools/call" {
            params.get("name")
        } else {
            params.get("taskId")
        }
        .and_then(Value::as_str)
        .expect("named tool or task request")
        .to_owned();
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).unwrap();
        let response = self
            .client
            .post(&self.url)
            .header("Accept", ACCEPT_BOTH)
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method)
            .header("Mcp-Name", name)
            .body(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string())
            .send()
            .await
            .expect("task HTTP request");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.expect("task HTTP body");
        let body = String::from_utf8_lossy(&bytes);
        assert!(status.is_success(), "HTTP task request failed: {status}: {body}");
        let response = parse_response_body(&headers, &bytes).expect("task HTTP JSON response");
        assert!(response.get("error").is_none(), "task JSON-RPC error: {response}");
        assert_eq!(response.get("id").and_then(Value::as_u64), Some(id));
        response
    }

    async fn command(&mut self, commands: &str) -> String {
        let response = self.request("tools/call", json!({"name": "lean_trial", "arguments": {"kind": "command", "imports": ["Init"], "commands": commands}})).await;
        task_id(&response).to_owned()
    }

    async fn terminal(&mut self, task_id: &str, timeout: Duration) -> Value {
        tokio::time::timeout(timeout, async {
            loop {
                let response = self.request("tasks/get", json!({"taskId": task_id})).await;
                if matches!(
                    response.pointer("/result/status").and_then(Value::as_str),
                    Some("completed" | "failed" | "cancelled")
                ) {
                    return response;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("task must reach a terminal state within its test deadline")
    }
}

fn task_id(response: &Value) -> &str {
    response
        .pointer("/result/taskId")
        .and_then(Value::as_str)
        .expect("task handle over HTTP")
}

fn sentinel_command(started: &Path, completion: Option<(&Path, u32)>) -> String {
    let started = serde_json::to_string(started.to_str().expect("UTF-8 test path")).unwrap();
    let mut command = format!("#eval do\n  IO.FS.writeFile {started} \"started\"\n");
    if let Some((finished, sleep_ms)) = completion {
        let finished = serde_json::to_string(finished.to_str().expect("UTF-8 test path")).unwrap();
        writeln!(
            &mut command,
            "  IO.sleep {sleep_ms}\n  IO.FS.writeFile {finished} \"finished\""
        )
        .unwrap();
    }
    command
}

async fn wait_for_sentinel(path: &Path, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Lean command must reach its test-owned start sentinel");
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
            .kill_on_drop(true)
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
        // Prefer the worker built alongside this test over the developer's
        // installed set. See `support::built_workers_dir`.
        if let Some(dir) = support::built_workers_dir(env!("CARGO_BIN_EXE_lean-host-mcp")) {
            command.env("LEAN_HOST_MCP_WORKERS_DIR", dir);
        }
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

    async fn modern_request(&self, method: &str, name: Option<&str>, version: &str, body: Value) -> HttpMcpResponse {
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", ACCEPT_BOTH)
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", version)
            .header("Mcp-Method", method)
            .body(body.to_string());
        if let Some(name) = name {
            request = request.header("Mcp-Name", name);
        }
        let response = request.send().await.unwrap();
        let http_status = response.status();
        let headers = response.headers().clone();
        let session_id = headers
            .get("Mcp-Session-Id")
            .map(|value| value.to_str().unwrap().to_owned());
        let json = parse_response_body(&headers, &response.bytes().await.unwrap()).unwrap();
        HttpMcpResponse {
            http_status,
            session_id,
            json,
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

fn success_runtime_u64(response: &Value, field: &str) -> Option<u64> {
    response
        .pointer("/result/structuredContent/telemetry/runtime")
        .and_then(|runtime| runtime.get(field))
        .and_then(Value::as_u64)
}

fn source_content_digest<'a>(response: &'a Value, path: &str) -> Option<&'a str> {
    response
        .pointer("/result/structuredContent/trust/artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|artifact| {
            artifact.get("artifact").and_then(Value::as_str) == Some("source")
                && artifact.get("path").and_then(Value::as_str) == Some(path)
                && artifact.get("status").and_then(Value::as_str) == Some("edit_fresh")
        })
        .and_then(|artifact| artifact.get("content_sha256"))
        .and_then(Value::as_str)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::with_capacity(64), |mut output, byte| {
        write!(&mut output, "{byte:02x}").expect("writing to a String is infallible");
        output
    })
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

#[tokio::test]
async fn streamable_http_drain_waits_then_resumes_without_client_retry() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lake-manifest.json"), "{}\n").unwrap();
    std::fs::write(root.path().join("lakefile.toml"), "name = \"control\"\n").unwrap();
    std::fs::write(root.path().join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
    let root_s = root.path().to_string_lossy();
    let server = HttpMcpServer::start(&[("--lake-root", root_s.as_ref())]).await;
    let mut owner = TaskHttpClient::new(&server.url);
    let acquired = owner.control(json!({"action": "drain", "retire_idle": true})).await;
    assert_eq!(acquired["status"], "acquired");
    assert_eq!(acquired["pool"]["workers_stopped"], true);
    assert!(
        acquired["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "draining_lease_v1")
    );
    let token = acquired["lease_id"].as_str().unwrap().to_owned();
    let mut session = server.new_session().await;
    let request = tokio::spawn(async move {
        session
            .request(
                "tools/call",
                json!({"name":"lean_trial", "arguments":{
                    "kind":"command", "imports":["Init"], "commands":"#check Nat"
                }}),
            )
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !request.is_finished(),
        "new request must wait rather than fail or overtake drain"
    );
    let released = owner.control(json!({"action":"release", "lease_id":token})).await;
    assert_eq!(released["status"], "ok");
    let response = tokio::time::timeout(Duration::from_secs(10), request)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        runtime_error_reason(&response.json).as_deref(),
        Some("worker_pool_draining")
    );
    assert_ne!(
        runtime_error_reason(&response.json).as_deref(),
        Some("worker_pool_leased")
    );
    server.shutdown().await;
}
