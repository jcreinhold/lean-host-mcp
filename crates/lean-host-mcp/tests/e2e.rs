//! Opt-in end-to-end test against any built Lake project (the
//! `lean-rs-host` shims live inside `lean-rs-host` itself and are
//! injected per session; consumers don't link them). Point
//! `LEAN_HOST_MCP_TEST_FIXTURE` at a built project to enable;
//! `fixtures/lean/` is the in-tree demo target.
//!
//! ```sh
//! cd /path/to/lean-host-mcp/fixtures/lean && lake build
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
//!     LEAN_HOST_MCP_TEST_PACKAGE=lean_rs_fixture \
//!     LEAN_HOST_MCP_TEST_LIBRARY=LeanRsFixture \
//!     cargo test --test e2e -- --ignored
//! ```

// Test code: `expect`, `unwrap`, and `panic!` are the idiomatic way to
// surface test failures and unreachable setup branches.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::lean::{ElaborateRequest, ElaborateResult, HoverByNameRequest, HoverByNameResult, elaborate, hover_by_name};
use lean_host_mcp::{BrokerConfig, ProjectBroker};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn open_ctx(root: &std::path::Path) -> ToolContext {
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let cache_path = cache_dir.keep();
    let broker = ProjectBroker::new(BrokerConfig {
        cache_dir: cache_path,
        config_default: None,
        env_default: Some(root.to_path_buf()),
        cwd: root.to_path_buf(),
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    ToolContext { broker }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn elaborate_prelude_term() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(Nat.succ 0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("elaborate returned");
    assert!(
        matches!(resp.result, ElaborateResult::Ok(_)),
        "elaboration should succeed: {:?}",
        resp.result
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn describe_prelude_name() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = hover_by_name(
        &ctx,
        HoverByNameRequest {
            name: "Nat.add_zero".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("hover_by_name");
    assert!(
        matches!(resp.result, HoverByNameResult::Found(_)),
        "Nat.add_zero is part of the prelude"
    );
}

#[test]
fn is_def_eq_request_round_trips_transparency() {
    use lean_host_mcp::tools::lean::{IsDefEqRequest, Transparency};

    let without: IsDefEqRequest = serde_json::from_str(r#"{"lhs":"1+1","rhs":"2"}"#).unwrap();
    assert!(without.transparency.is_none());

    let with: IsDefEqRequest = serde_json::from_str(r#"{"lhs":"1+1","rhs":"2","transparency":"reducible"}"#).unwrap();
    assert!(matches!(with.transparency, Some(Transparency::Reducible)));
}

#[test]
fn find_symbol_request_round_trips() {
    use lean_host_mcp::tools::index::FindSymbolRequest;

    let minimal: FindSymbolRequest = serde_json::from_str(r#"{"query":"add_zero"}"#).unwrap();
    assert_eq!(minimal.query, "add_zero");
    assert!(minimal.imports.is_empty());
    assert!(minimal.limit.is_none());

    let full: FindSymbolRequest = serde_json::from_str(r#"{"query":"map","imports":["List"],"limit":25}"#).unwrap();
    assert_eq!(full.limit, Some(25));
    assert_eq!(full.imports, vec!["List".to_owned()]);
}

#[test]
fn outline_request_accepts_module_prefix() {
    use lean_host_mcp::tools::index::OutlineRequest;

    let nat: OutlineRequest = serde_json::from_str(r#"{"module_prefix":"Nat."}"#).unwrap();
    assert_eq!(nat.module_prefix.as_deref(), Some("Nat."));

    let none: OutlineRequest = serde_json::from_str("{}").unwrap();
    assert!(none.module_prefix.is_none());
}

#[test]
fn position_requests_round_trip() {
    use lean_host_mcp::tools::position::{GoalAtPositionRequest, ReferencesOfNameRequest, TypeAtPositionRequest};

    let g: GoalAtPositionRequest = serde_json::from_str(r#"{"file":"Foo/Bar.lean","line":7,"column":3}"#).unwrap();
    assert_eq!(g.line, 7);
    assert_eq!(g.column, 3);

    // A caller may still send an `imports` field; serde silently ignores
    // unknown fields by default. The schema no longer publishes it.
    let t: TypeAtPositionRequest =
        serde_json::from_str(r#"{"file":"X.lean","line":1,"column":1,"imports":["A.B"]}"#).unwrap();
    assert_eq!(t.line, 1);

    let r_default: ReferencesOfNameRequest = serde_json::from_str(r#"{"name":"Nat.add"}"#).unwrap();
    assert!(r_default.files.is_empty());

    let r_full: ReferencesOfNameRequest =
        serde_json::from_str(r#"{"name":"Nat.add","files":["A.lean","B.lean"]}"#).unwrap();
    assert_eq!(r_full.files.len(), 2);
}

#[test]
fn references_result_skips_empty_fields() {
    use lean_host_mcp::tools::position::ReferencesOfNameResult;

    let empty = ReferencesOfNameResult {
        references: Vec::new(),
        truncated: false,
        unsupported_files: Vec::new(),
        header_parse_failed_files: Vec::new(),
        missing_imports_files: Vec::new(),
    };
    let s = serde_json::to_string(&empty).unwrap();
    assert!(!s.contains("truncated"), "truncated=false must be omitted: {s}");
    assert!(
        !s.contains("unsupported_files"),
        "empty unsupported_files must be omitted: {s}"
    );
    assert!(
        !s.contains("header_parse_failed_files"),
        "empty header_parse_failed_files must be omitted: {s}"
    );
    assert!(
        !s.contains("missing_imports_files"),
        "empty missing_imports_files must be omitted: {s}"
    );

    let with_flags = ReferencesOfNameResult {
        references: Vec::new(),
        truncated: true,
        unsupported_files: vec!["A.lean".into()],
        header_parse_failed_files: Vec::new(),
        missing_imports_files: Vec::new(),
    };
    let s = serde_json::to_string(&with_flags).unwrap();
    assert!(s.contains("\"truncated\":true"));
    assert!(s.contains("\"unsupported_files\":[\"A.lean\"]"));
}

#[test]
fn goal_result_serialises_status_tag() {
    use lean_host_mcp::tools::position::GoalAtPositionResult;

    let s = serde_json::to_string(&GoalAtPositionResult::NoTacticContext).unwrap();
    assert_eq!(s, r#"{"status":"no_tactic_context"}"#);

    let s = serde_json::to_string(&GoalAtPositionResult::Unsupported).unwrap();
    assert_eq!(s, r#"{"status":"unsupported"}"#);
}

#[test]
fn file_diagnostics_request_round_trips() {
    use lean_host_mcp::tools::position::FileDiagnosticsRequest;

    let r: FileDiagnosticsRequest = serde_json::from_str(r#"{"file":"Foo/Bar.lean"}"#).unwrap();
    assert_eq!(r.file, PathBuf::from("Foo/Bar.lean"));

    // Unknown fields are ignored, same as the cursor-driven requests.
    let r2: FileDiagnosticsRequest = serde_json::from_str(r#"{"file":"X.lean","line":1}"#).unwrap();
    assert_eq!(r2.file, PathBuf::from("X.lean"));
}

#[test]
fn file_diagnostics_result_serialises_status_tag() {
    use lean_host_mcp::tools::position::{DiagnosticSummary, FileDiagnosticsResult};

    let s = serde_json::to_string(&FileDiagnosticsResult::Unsupported).unwrap();
    assert_eq!(s, r#"{"status":"unsupported"}"#);

    let s = serde_json::to_string(&FileDiagnosticsResult::Elaborated {
        summary: DiagnosticSummary::default(),
        diagnostics: Vec::new(),
        truncated: false,
    })
    .unwrap();
    assert_eq!(
        s,
        r#"{"status":"elaborated","summary":{"errors":0,"warnings":0,"info":0},"diagnostics":[],"truncated":false}"#
    );

    let s = serde_json::to_string(&FileDiagnosticsResult::HeaderParseFailed {
        summary: DiagnosticSummary::default(),
        diagnostics: Vec::new(),
        truncated: false,
    })
    .unwrap();
    assert_eq!(
        s,
        r#"{"status":"header_parse_failed","summary":{"errors":0,"warnings":0,"info":0},"diagnostics":[],"truncated":false}"#
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn hover_by_name_populates_type_signature() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = hover_by_name(
        &ctx,
        HoverByNameRequest {
            name: "Nat.add_zero".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("hover_by_name");
    let HoverByNameResult::Found(row) = resp.result else {
        panic!("Nat.add_zero must be present");
    };
    assert!(
        row.type_signature.is_some(),
        "expr_to_string_raw should yield a type for Nat.add_zero"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_returns_clean_file_empty() {
    use lean_host_mcp::tools::position::{FileDiagnosticsRequest, FileDiagnosticsResult, file_diagnostics};

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file: PathBuf::from("LeanRsFixture/SourceRanges.lean"),
            project: None,
        },
    )
    .await
    .expect("file_diagnostics");
    let FileDiagnosticsResult::Elaborated {
        summary, diagnostics, ..
    } = resp.result
    else {
        panic!("expected Elaborated variant, got something else");
    };
    assert_eq!(
        summary.errors, 0,
        "clean fixture should record no error-severity diagnostics; got {diagnostics:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_returns_real_errors() {
    use std::io::Write;

    use lean_host_mcp::Severity;
    use lean_host_mcp::tools::position::{FileDiagnosticsRequest, FileDiagnosticsResult, file_diagnostics};

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    // Two-line file so the error's line number is unambiguous.
    let mut tmp = tempfile::NamedTempFile::with_suffix(".lean").expect("tempfile");
    writeln!(tmp, "-- broken file").unwrap();
    writeln!(tmp, "theorem broken : 1 + 1 = 3 := rfl").unwrap();
    tmp.flush().unwrap();

    let resp = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file: tmp.path().to_path_buf(),
            project: None,
        },
    )
    .await
    .expect("file_diagnostics");
    let FileDiagnosticsResult::Elaborated {
        summary, diagnostics, ..
    } = resp.result
    else {
        panic!("expected Elaborated variant with diagnostics; got something else");
    };
    assert!(
        summary.errors >= 1,
        "summary.errors must reflect the deliberate failure; got {summary:?}"
    );
    let error = diagnostics
        .iter()
        .find(|d| matches!(d.severity, Severity::Error))
        .expect("at least one error-severity diagnostic for `1 + 1 = 3 := rfl`");
    let pos = error.position.as_ref().expect("error diagnostic has a position");
    assert_eq!(pos.line, 2, "error must be reported on the theorem line (got {pos:?})");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_cache_warm() {
    use lean_host_mcp::tools::position::{
        FileDiagnosticsRequest, GoalAtPositionRequest, file_diagnostics, goal_at_position,
    };

    const HINT_FRAGMENT: &str = "file processed and cached";

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let file = PathBuf::from("LeanRsFixture/SourceRanges.lean");

    // Cold: must record the cache-and-process hint.
    let cold = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file: file.clone(),
            project: None,
        },
    )
        .await
        .expect("cold file_diagnostics");
    assert!(
        cold.next_actions.iter().any(|n| n.contains(HINT_FRAGMENT)),
        "cold call should attach the cache hint; next_actions={:?}",
        cold.next_actions
    );

    // Different tool, same file: must hit the shared cache (no hint).
    let probe = goal_at_position(
        &ctx,
        GoalAtPositionRequest {
            file: file.clone(),
            line: 1,
            column: 1,
            project: None,
        },
    )
    .await
    .expect("goal_at_position");
    assert!(
        probe.next_actions.iter().all(|n| !n.contains(HINT_FRAGMENT)),
        "second tool call against the cached file must not re-process; next_actions={:?}",
        probe.next_actions
    );

    // Warm: same tool, still cached.
    let warm = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file,
            project: None,
        },
    )
        .await
        .expect("warm file_diagnostics");
    assert!(
        warm.next_actions.iter().all(|n| !n.contains(HINT_FRAGMENT)),
        "warm file_diagnostics call must not re-process; next_actions={:?}",
        warm.next_actions
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn index_rebuilds_and_finds_prelude_theorem() {
    use lean_host_mcp::tools::index::{FindLemmaRequest, find_lemma};

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = find_lemma(
        &ctx,
        FindLemmaRequest {
            query: "add_zero".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            limit: Some(50),
            project: None,
        },
    )
    .await
    .expect("find_lemma");
    assert!(
        resp.result.iter().any(|d| d.name == "Nat.add_zero"),
        "Nat.add_zero must be reachable through find_lemma after rebuild"
    );
}

#[test]
fn envelope_serialises() {
    use lean_host_mcp::{Freshness, Response};
    let r = Response::ok(
        serde_json::json!({"foo": 1}),
        Freshness {
            project_root: "/tmp/x".into(),
            project_hash: "deadbeef".into(),
            imports: vec!["A.B".into()],
            session_id: "abc".into(),
            lean_toolchain: "leanprover/lean4:v4.29.1".into(),
        },
    );
    let s = serde_json::to_string(&r).unwrap();
    assert!(s.contains("\"foo\":1"));
    assert!(s.contains("\"project_root\":\"/tmp/x\""));
    assert!(s.contains("\"project_hash\":\"deadbeef\""));
    assert!(!s.contains("\"warnings\""), "empty warnings should be omitted");
}
