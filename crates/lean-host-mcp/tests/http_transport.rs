//! Black-box Streamable HTTP transport tests.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};

const ACCEPT_BOTH: &str = "application/json, text/event-stream";
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const EXPECTED_TOOLS: [&str; 10] = [
    "lean_build",
    "lean_context",
    "lean_get",
    "lean_imports",
    "lean_lookup",
    "lean_run",
    "lean_runtime",
    "lean_status",
    "lean_trial",
    "lean_verify",
];
const REMOVED_TOOLS: [&str; 7] = [
    "proof_state",
    "search_for_proof",
    "inspect_declaration",
    "try_proof_step",
    "verify_declaration",
    "find_references",
    "lean_compatibility",
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
        // Every tool but `lean_runtime` takes a request id: `lean_run` and
        // `lean_get` need one, and the others take one to run as a request.
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        let request_id = tool.pointer("/inputSchema/properties/request_id");
        match name {
            "lean_runtime" => assert!(request_id.is_none(), "lean_runtime takes no request_id: {tool:?}"),
            "lean_run" | "lean_get" => assert!(
                tool.pointer("/inputSchema/required")
                    .and_then(Value::as_array)
                    .is_some_and(|required| required.contains(&json!("request_id"))),
                "tool {name} must require a request_id: {tool:?}"
            ),
            _ => assert_eq!(
                request_id.and_then(|id| id.get("pattern")).and_then(Value::as_str),
                Some("^[A-Za-z0-9._:-]{8,128}$"),
                "tool {name} must take a request_id: {tool:?}"
            ),
        }
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
    assert!(
        discover
            .json
            .pointer("/result/capabilities/extensions/io.modelcontextprotocol~1tasks")
            .is_none(),
        "the server offers no MCP Tasks: {discover:?}"
    );

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
    // Method not found: requests live in `lean_run` and `lean_get`.
    assert_eq!(task.json.pointer("/error/code").and_then(Value::as_i64), Some(-32601));
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
            .env_remove("LEAN_HOST_MCP_PROJECT")
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

/// Workers refuse an inherited `LEAN_PATH`, so a server started with one
/// would fail every check. It refuses to start instead, before opening state.
#[tokio::test]
async fn an_inherited_lean_path_refuses_startup() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().canonicalize().unwrap();
    let store = root.join("store");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
    command
        .kill_on_drop(true)
        .current_dir(&root)
        .arg("--bind")
        .arg(reserve_loopback_addr().to_string())
        .arg("--lake-root")
        .arg(&root)
        .env("LEAN_PATH", "/nowhere")
        .env("LEAN_HOST_MCP_STORE_DIR", &store)
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("a server with LEAN_PATH set must exit before serving")
        .expect("run lean-host-mcp with LEAN_PATH set");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("LEAN_PATH is set"), "{stderr}");
    assert!(!store.exists(), "refused startup opened {}", store.display());
}

#[tokio::test]
async fn second_server_on_one_store_names_the_first() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let store = scratch.path().join("store");
    let root_s = root.to_str().unwrap();
    let first = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s)],
        &[("LEAN_HOST_MCP_STORE_DIR", store.to_str().unwrap())],
    )
    .await;

    let output = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--lake-root")
        .arg(&root)
        .env("LEAN_HOST_MCP_STORE_DIR", &store)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(10), output)
        .await
        .expect("the second server must exit instead of serving")
        .expect("run the second server");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    let expected = format!(
        "another lean-host-mcp (pid {}, lake root {}) holds the store {}; stop it first",
        first.pid(),
        root.display(),
        store.display()
    );
    assert!(stderr.contains(&expected), "expected `{expected}` in:\n{stderr}");
    first.shutdown().await;
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
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
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

/// Install, then serve, a copy of the fixture with `HOME` set to a new empty
/// directory. A build, a check and a proof search must leave nothing under
/// `HOME` and nothing in `git status`. Each is a direct call with no
/// `request_id`, so each reply names a server-made `srv-` id.
///
/// `ELAN_HOME`, `CARGO_HOME` and `RUSTUP_HOME` keep their real values, and
/// install builds the release worker offline, so install downloads nothing.
#[tokio::test]
#[ignore = "builds the release worker"]
async fn install_and_serve_keep_no_state_in_an_empty_home() {
    let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let real = |name: &str, default: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map_or_else(|| real_home.join(default), PathBuf::from)
    };
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let scratch = tempfile::Builder::new()
        .prefix("empty-home-")
        .tempdir_in(target_tmp)
        .unwrap();
    let scratch_dir = scratch.path().canonicalize().unwrap();
    let home = scratch_dir.join("home");
    let tmp = scratch_dir.join("tmp");
    let project = scratch_dir.join("project");
    for dir in [&home, &tmp, &project] {
        std::fs::create_dir(dir).unwrap();
    }
    copy_fixture(&fixture_root(), &project);
    std::fs::write(project.join(".gitignore"), "/.lake/\n").unwrap();

    let envs: Vec<(&str, std::ffi::OsString)> = vec![
        ("PATH", std::env::var_os("PATH").expect("PATH")),
        ("HOME", home.clone().into()),
        ("ELAN_HOME", real("ELAN_HOME", ".elan").into()),
        ("CARGO_HOME", real("CARGO_HOME", ".cargo").into()),
        ("RUSTUP_HOME", real("RUSTUP_HOME", ".rustup").into()),
        ("TMPDIR", tmp.into()),
        ("CARGO_NET_OFFLINE", "true".into()),
        ("NO_COLOR", "1".into()),
    ];
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=lean-host-mcp", "-c", "user.email=test@example.invalid"])
            .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(&project)
            .env_clear()
            .envs(envs.iter().cloned())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "fixture"]);

    let install = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .arg("install")
        .arg("--lake-root")
        .arg(&project)
        .env_clear()
        .envs(envs.iter().cloned())
        .stdin(Stdio::null())
        .output();
    let install = tokio::time::timeout(Duration::from_mins(30), install)
        .await
        .expect("install must finish")
        .expect("run install");
    assert!(
        install.status.success(),
        "install failed:\n{}{}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );

    // Serve as `make mcp-serve` does: the installed binary and the Lake root.
    let local = project.join(".local/lean-host-mcp");
    let config: toml::Table = std::fs::read_to_string(local.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let binary = config["install"]["binary"].as_str().expect("installed binary");
    let mut command = Command::new(binary);
    command
        .kill_on_drop(true)
        .arg("--lake-root")
        .arg(&project)
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--http-path")
        .arg("/mcp")
        .current_dir(&project)
        .env_clear()
        .envs(envs.iter().cloned())
        .env("RUST_LOG", "warn,lean_host_mcp::transport_http=info")
        .env("LEAN_HOST_MCP_RESPONSE_CARRIER", "both")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let server = HttpMcpServer::spawn(command, None).await;
    let mut session = server.new_session().await;
    // The installed worker serves no project request, so the semantic call is
    // a proof search, which a short run serves.
    for (name, arguments) in [
        ("lean_build", json!({})),
        (
            "lean_verify",
            json!({"targets": [{
                "kind": "explicit",
                "file": "LeanRsFixture/ProofActions.lean",
                "declarations": ["LeanRsFixture.ProofActions.stepTheorem"]
            }]}),
        ),
        (
            "lean_lookup",
            json!({
                "kind": "proof_search",
                "file": "LeanRsFixture/ProofAgent.lean",
                "declaration": "LeanRsFixture.ProofAgent.miniRatDenominatorStep",
                "limit": 5
            }),
        ),
    ] {
        let response = session
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_ne!(
            response.json.pointer("/result/isError"),
            Some(&Value::Bool(true)),
            "{name} failed: {}",
            response.json
        );
        assert!(
            response
                .json
                .pointer("/result/structuredContent/request_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("srv-")),
            "{name} names no server id: {}",
            response.json
        );
    }
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(server.pid().to_string())
        .status()
        .expect("send SIGTERM");
    assert!(status.success(), "kill -TERM: {status}");
    server.wait_for_exit().await;

    assert_eq!(
        git(&["status", "--porcelain", "--untracked-files=all"]),
        "",
        "git status after install, serve and a build"
    );
    // The state that once went under `HOME` is in the store, with the
    // request table.
    for dir in ["children", "semantic-runtimes", "processes"] {
        assert!(local.join("store").join(dir).is_dir(), "the store has no {dir}/");
    }
    assert!(
        local.join("store/requests.sqlite3").is_file(),
        "the store has no request table"
    );
    let left = list_tree(&home);
    eprintln!("under the empty HOME: {left:?}");
    assert!(left.is_empty(), "install and serve wrote under HOME: {left:?}");
}

/// Copy the fixture's sources from `from` into the existing directory `to`,
/// leaving out Lake's build directory.
fn copy_fixture(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".lake" {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir(&target).unwrap();
            copy_fixture(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Every path under `root`, relative to it, in order.
fn list_tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
            paths.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
        }
    }
    paths.sort();
    paths
}

#[tokio::test]
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
async fn streamable_http_concurrent_sessions_surface_structured_pressure() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    // One project in the pool, so all six sessions contend for one worker's
    // mailbox — the server's only admission mechanism.
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_str())],
        &[
            ("LEAN_HOST_MCP_MAX_WORKERS", "1"),
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
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
async fn streamable_http_five_clients_check_the_same_source_bytes() {
    let root = fixture_root();
    let root_s = root.to_string_lossy().into_owned();
    let expected =
        sha256_hex(&std::fs::read(root.join("LeanRsFixture/ProofActions.lean")).expect("read fixture source"));
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_str())],
        &[
            ("LEAN_HOST_MCP_MAX_WORKERS", "1"),
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
    }
    server.shutdown().await;
}

#[tokio::test]
async fn streamable_http_status_reports_resolved_settings_without_opening_worker() {
    let root = tempfile::tempdir().unwrap();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root.path().to_str().unwrap())],
        &[
            ("LEAN_HOST_MCP_MAX_WORKERS", "2"),
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
    let client = StatelessClient::new(&server.url);
    let status = client.control(json!({})).await;
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
        json!({"max_workers":2,"max_workers_per_project":1,"idle_timeout_secs":91})
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
    assert_eq!(status["requests"], json!({"queued": 0, "running": 0}));
    assert_eq!(status["capabilities"], json!(["durable_requests_v1"]));
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
async fn streamable_http_named_requests_survive_conflicts_and_dropped_calls() {
    let scratch = tempfile::tempdir().unwrap();
    let running = scratch.path().join("running");
    let finished = scratch.path().join("finished");
    let dropped = scratch.path().join("dropped-call-must-not-run");
    let first = scratch.path().join("first");
    let last = scratch.path().join("last");
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[("LEAN_HOST_MCP_MAX_WORKERS", "1")],
    )
    .await;
    let client = StatelessClient::new(&server.url);
    client
        .command("blocker-0001", &sentinel_command(&running, Some((&finished, 4_000))))
        .await;
    wait_for_sentinel(&running, Duration::from_secs(30)).await;
    let conflict = client.run("blocker-0001", "lean_trial", trial("#eval (1 : Nat)")).await;
    assert_eq!(conflict["error"]["code"], "request_id_conflict", "{conflict}");
    assert_eq!(client.get("blocker-0001").await["request"]["state"], "running");
    assert!(
        !finished.exists(),
        "a retry with other arguments cannot stop or replace the running command"
    );
    client.command("first-0001", &sentinel_command(&first, None)).await;
    // A direct call queues behind the blocker; then its client goes away.
    let call = client.spawn_call("lean_trial", trial(&sentinel_command(&dropped, None)));
    client.wait_for_jobs(3).await;
    call.abort();
    client.command("last-0001", &sentinel_command(&last, None)).await;
    assert!(!finished.exists());
    let other = StatelessClient::new(&server.url);
    for id in ["blocker-0001", "first-0001", "last-0001"] {
        let reply = other.ended(id, Duration::from_secs(30)).await;
        assert_eq!(reply["request"]["state"], "done", "{reply}");
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        assert!(
            reply.pointer("/result/structuredContent/data/output").is_some(),
            "{reply}"
        );
    }
    assert!(finished.exists());
    assert!(first.exists());
    assert!(last.exists());
    assert!(!dropped.exists(), "a dropped queued call must never enter Lean");
    let healthy = scratch.path().join("healthy-follow-up");
    let follow_up = other.call("lean_trial", trial(&sentinel_command(&healthy, None))).await;
    assert_eq!(follow_up["result"]["isError"], false, "{follow_up}");
    assert!(healthy.exists(), "the worker must stay usable: {follow_up}");
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
async fn streamable_http_dropped_running_call_stops_lean_and_drains() {
    let scratch = tempfile::tempdir().expect("test-owned sentinels");
    let started = scratch.path().join("started");
    let forbidden = scratch.path().join("ran-after-the-client-left");
    let root = fixture_root();
    let root_s = root.to_string_lossy();
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[("LEAN_HOST_MCP_MAX_WORKERS", "1")],
    )
    .await;
    let client = StatelessClient::new(&server.url);
    let call = client.spawn_call(
        "lean_trial",
        trial(&sentinel_command(&started, Some((&forbidden, 4_000)))),
    );
    wait_for_sentinel(&started, Duration::from_secs(30)).await;
    assert!(!forbidden.exists(), "the client must leave before Lean finishes");
    call.abort();
    let healthy = scratch.path().join("healthy-follow-up");
    let response = client
        .call("lean_trial", trial(&sentinel_command(&healthy, None)))
        .await;
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert!(
        healthy.exists(),
        "the worker must accept work after the dropped call drains: {response}"
    );
    assert!(
        response
            .pointer("/result/structuredContent/request_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("srv-")),
        "{response}"
    );
    // Catch Lean work that survives its dropped call.
    tokio::time::sleep(Duration::from_millis(4_250)).await;
    assert!(
        !forbidden.exists(),
        "the dropped call's Lean command went on in the background"
    );
    server.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "needs the built Lean fixture and an installed worker: set LEAN_HOST_MCP_WORKERS_DIR"]
async fn streamable_http_dropped_startup_preserves_coalesced_worker_at_capacity() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = tempfile::Builder::new()
        .prefix("lean-startup-'wrapper-")
        .tempdir()
        .expect("test-owned delayed worker");
    let root = fixture_root();
    let toolchain = lean_host_mcp::ToolchainId::from_lake_root(&root).expect("fixture toolchain");
    let installed = support::installed_workers_dir().expect("installed worker");
    let worker = lean_host_mcp::toolchain::WorkerBinary::resolve_in(&toolchain, &installed)
        .expect("installed worker for the fixture's toolchain")
        .path
        .canonicalize()
        .expect("exact installed worker path");
    let starts = scratch.path().join("worker-starts");
    let release = scratch.path().join("release-startup");
    let wrapper = scratch.path().join("lean-host-mcp-worker");
    // Delay the worker handshake, not a Lean command, so the first caller
    // still holds the project's opening lock when its client leaves. Bound
    // the wait so a failed assertion cannot leave a child stuck. Each caller
    // that finds no worker first runs this file with the readiness flag,
    // before it takes the opening lock. That run checks the installed shims
    // and opens no Lean, so it goes straight through, and `starts` counts
    // worker children only. The server runs a worker file at the top of its
    // worker directory as it finds it, and the installed worker it execs
    // checks its own installation.
    let script = format!(
        "#!/bin/sh\n[ \"$1\" != --lean-rs-ready-host-artifacts ] || exec {worker} \"$@\"\nprintf '%s\\n' \"$$\" >> {starts}\nremaining=500\nwhile [ ! -f {release} ]; do\n  remaining=$((remaining - 1))\n  [ \"$remaining\" -gt 0 ] || exit 75\n  sleep 0.01\ndone\nexec {worker} \"$@\"\n",
        starts = shell_quote_path(&starts),
        release = shell_quote_path(&release),
        worker = shell_quote_path(&worker),
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root_s = root.to_string_lossy();
    let wrapper_dir = scratch.path().to_str().expect("UTF-8 test path");
    let server = HttpMcpServer::start_with_env(
        &[("--lake-root", root_s.as_ref())],
        &[
            ("LEAN_HOST_MCP_MAX_WORKERS", "1"),
            ("LEAN_HOST_MCP_WORKERS_DIR", wrapper_dir),
        ],
    )
    .await;
    let opener = StatelessClient::new(&server.url);
    let forbidden = scratch.path().join("dropped-opener-entered-lean");
    let opening = opener.spawn_call("lean_trial", trial(&sentinel_command(&forbidden, None)));
    wait_for_sentinel(&starts, Duration::from_secs(30)).await;
    let waiter = StatelessClient::new(&server.url);
    let healthy = scratch.path().join("surviving-waiter-entered-lean");
    waiter.command("waiter-0001", &sentinel_command(&healthy, None)).await;
    waiter.wait_for_state("waiter-0001", "running").await;
    assert!(!healthy.exists(), "a same-project request must wait for startup");
    opening.abort();
    assert_eq!(
        std::fs::read_to_string(&starts).unwrap().lines().count(),
        1,
        "only one worker may start while the capacity-one startup is held"
    );
    std::fs::write(&release, "continue").unwrap();
    let completed = waiter.ended("waiter-0001", Duration::from_secs(30)).await;
    assert_eq!(
        completed["request"]["state"], "done",
        "the surviving caller must reuse the new worker rather than report a busy pool: {completed}"
    );
    assert_eq!(completed["result"]["isError"], false, "{completed}");
    assert!(healthy.exists(), "the surviving caller must run Lean: {completed}");
    assert!(!forbidden.exists(), "the dropped opener must not run its Lean command");
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

/// A client of the stateless protocol: one HTTP request per call, no session,
/// no tasks.
#[derive(Clone)]
struct StatelessClient {
    client: reqwest::Client,
    url: String,
    next_id: Arc<AtomicU64>,
}

impl StatelessClient {
    fn new(url: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(35))
                .build()
                .unwrap(),
            url: url.to_owned(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Call `tool` and return the JSON-RPC response, which holds `result` or
    /// `error`.
    async fn send(&self, tool: &str, arguments: Value) -> Value {
        use rmcp::model::{ClientCapabilities, Implementation, ProtocolVersion, RequestMetaObject};
        let meta = RequestMetaObject::with_client_context(
            ProtocolVersion::V_2026_07_28,
            Implementation::new("lean-host-lifecycle-test", "1"),
            ClientCapabilities::default(),
        );
        let params = json!({"name": tool, "arguments": arguments, "_meta": meta});
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .client
            .post(&self.url)
            .header("Accept", ACCEPT_BOTH)
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", tool)
            .body(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}).to_string())
            .send()
            .await
            .expect("HTTP request");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.expect("HTTP body");
        let body = String::from_utf8_lossy(&bytes);
        assert!(status.is_success(), "HTTP request failed: {status}: {body}");
        let response = parse_response_body(&headers, &bytes).expect("JSON-RPC response");
        assert_eq!(response.get("id").and_then(Value::as_u64), Some(id));
        response
    }

    /// Call `tool` and return the JSON-RPC response. Fails on a JSON-RPC
    /// error.
    async fn call(&self, tool: &str, arguments: Value) -> Value {
        let response = self.send(tool, arguments).await;
        assert!(response.get("error").is_none(), "JSON-RPC error: {response}");
        response
    }

    /// The structured content of a call to `tool`.
    async fn structured(&self, tool: &str, arguments: Value) -> Value {
        let response = self.call(tool, arguments).await;
        response
            .pointer("/result/structuredContent")
            .unwrap_or_else(|| panic!("{tool} returned no structured content: {response}"))
            .clone()
    }

    async fn control(&self, arguments: Value) -> Value {
        self.structured("lean_runtime", arguments).await
    }

    /// `lean_run` request `id`: the structured reply.
    async fn run(&self, id: &str, tool: &str, arguments: Value) -> Value {
        self.structured(
            "lean_run",
            json!({"request_id": id, "tool": tool, "arguments": arguments}),
        )
        .await
    }

    /// Start request `id`, a `lean_trial` of `commands`.
    async fn command(&self, id: &str, commands: &str) {
        let reply = self.run(id, "lean_trial", trial(commands)).await;
        assert!(reply.get("request").is_some(), "lean_run {id} made no row: {reply}");
    }

    /// `lean_get` of request `id`, waiting up to `wait_seconds`:
    /// `{request, result}`.
    async fn get_within(&self, id: &str, wait_seconds: u64) -> Value {
        self.structured("lean_get", json!({"request_id": id, "wait_seconds": wait_seconds}))
            .await
    }

    async fn get(&self, id: &str) -> Value {
        self.get_within(id, 0).await
    }

    /// Wait up to `timeout` until request `id` ends; `lean_get`'s reply.
    async fn ended(&self, id: &str, timeout: Duration) -> Value {
        tokio::time::timeout(timeout, async {
            loop {
                let reply = self.get_within(id, 1).await;
                if matches!(
                    reply.pointer("/request/state").and_then(Value::as_str),
                    Some("done" | "timed_out" | "interrupted")
                ) {
                    return reply;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("request {id} must end within {timeout:?}"))
    }

    /// Wait until request `id` reaches `state`.
    async fn wait_for_state(&self, id: &str, state: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.get(id).await["request"]["state"] != state {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("request {id} never reached {state}"));
    }

    /// Wait until the pool counts at least `jobs` outstanding jobs.
    async fn wait_for_jobs(&self, jobs: u64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = self.control(json!({})).await;
                if status["pool"]["outstanding_jobs"]
                    .as_u64()
                    .is_some_and(|count| count >= jobs)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the pool never held {jobs} jobs"));
    }

    /// A direct call of `tool` in a task of its own. Aborting the task drops
    /// the call's connection, as a client that goes away does.
    fn spawn_call(&self, tool: &'static str, arguments: Value) -> tokio::task::JoinHandle<Value> {
        let client = self.clone();
        tokio::spawn(async move { client.send(tool, arguments).await })
    }
}

/// `lean_trial` arguments that run `commands` after importing `Init`.
fn trial(commands: &str) -> Value {
    json!({"kind": "command", "imports": ["Init"], "commands": commands})
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
    /// The server's own store, so tests on one Lake root do not share its
    /// lock, and its process registry. A test that passes
    /// `LEAN_HOST_MCP_STORE_DIR` replaces the store; an installed server
    /// keeps its store under its Lake root.
    _scratch: Option<tempfile::TempDir>,
}

impl HttpMcpServer {
    async fn start(args: &[(&str, &str)]) -> Self {
        Self::start_with_env(args, &[]).await
    }

    async fn start_with_env(args: &[(&str, &str)], envs: &[(&str, &str)]) -> Self {
        let store = tempfile::tempdir().expect("temp store dir");
        let mut command = Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"));
        command
            .kill_on_drop(true)
            .arg("--bind")
            .arg("127.0.0.1:0")
            .env("LEAN_HOST_MCP_STORE_DIR", store.path())
            // A server without `--lake-root` has no store, so it would keep
            // its process record in the user's cache, and a killed test
            // server never removes it.
            .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", store.path().join("processes"))
            .env("RUST_LOG", "warn,lean_host_mcp::transport_http=info")
            .env("NO_COLOR", "1")
            // These tests read `structuredContent` and full telemetry; opt into both.
            .env("LEAN_HOST_MCP_RESPONSE_CARRIER", "both")
            .env("LEAN_HOST_MCP_TELEMETRY_VERBOSITY", "full")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // The server inherits the caller's `LEAN_HOST_MCP_WORKERS_DIR`, as in
        // `support::http`. The tests that start workers need it to name an
        // installed one: see `support::installed_workers_dir`.
        for (key, value) in args {
            command.arg(key).arg(value);
        }
        for (key, value) in envs {
            command.env(key, value);
        }
        Self::spawn(command, Some(store)).await
    }

    /// Spawn `command`, a server that binds `127.0.0.1:0` with stderr piped
    /// and logs `lean_host_mcp::transport_http` at info, and wait for the
    /// address it listens on.
    async fn spawn(mut command: Command, scratch: Option<tempfile::TempDir>) -> Self {
        let mut child = command.spawn().expect("spawn lean-host-mcp HTTP server");
        let stderr = child.stderr.take().expect("server stderr");
        let mut stderr = BufReader::new(stderr);
        let mut startup = String::new();
        let bind: SocketAddr = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                let bytes = stderr.read_line(&mut line).await.expect("read server startup");
                assert!(bytes > 0, "HTTP server exited before listening:\n{startup}");
                startup.push_str(&line);
                if line.contains("Streamable HTTP transport listening") {
                    return line
                        .split_whitespace()
                        .find_map(|field| field.strip_prefix("bind="))
                        .expect("actual HTTP bind address")
                        .parse()
                        .expect("valid HTTP bind address");
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("HTTP server did not announce its listening address:\n{startup}"));
        let stderr_task = tokio::spawn(read_stderr(stderr, startup));

        Self {
            child,
            stderr_task,
            client: reqwest::Client::new(),
            url: format!("http://{bind}/mcp"),
            _scratch: scratch,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id().expect("running HTTP test server has a pid")
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

async fn read_stderr(mut reader: BufReader<ChildStderr>, mut out: String) -> String {
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
