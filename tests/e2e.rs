//! Opt-in end-to-end test against a built Lake project with `lean-rs-host`
//! shims. v0.1 does not ship a bundled fixture (the shim contract is
//! non-trivial); point `LEAN_HOST_MCP_TEST_FIXTURE` at a built project to
//! enable.
//!
//! ```sh
//! cd /path/to/lean-rs/fixtures/lean && lake build
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
//!     LEAN_HOST_MCP_TEST_PACKAGE=lean_rs_fixture \
//!     LEAN_HOST_MCP_TEST_LIBRARY=LeanRsFixture \
//!     cargo test --test e2e -- --ignored
//! ```

// Test code: `expect`, `unwrap`, and `panic!` are the idiomatic way to
// surface test failures and unreachable setup branches.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use lean_host_mcp::SessionHost;
use lean_rs_host as _;

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn elaborate_prelude_term() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let result = host
        .elaborate("(Nat.succ 0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("elaborate returned");
    assert!(result.is_ok(), "elaboration should succeed: {result:?}");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn describe_prelude_name() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let row = host
        .describe("Nat.add_zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("describe");
    assert!(row.is_some(), "Nat.add_zero is part of the prelude");
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
    use lean_host_mcp::tools::position::FileDiagnosticsResult;

    let s = serde_json::to_string(&FileDiagnosticsResult::Unsupported).unwrap();
    assert_eq!(s, r#"{"status":"unsupported"}"#);

    let s = serde_json::to_string(&FileDiagnosticsResult::Ok {
        diagnostics: Vec::new(),
        truncated: false,
    })
    .unwrap();
    assert_eq!(s, r#"{"status":"ok","diagnostics":[],"truncated":false}"#);

    let s = serde_json::to_string(&FileDiagnosticsResult::HeaderParseFailed {
        diagnostics: Vec::new(),
        truncated: false,
    })
    .unwrap();
    assert_eq!(
        s,
        r#"{"status":"header_parse_failed","diagnostics":[],"truncated":false}"#
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn hover_by_name_populates_type_signature() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let row = host
        .describe("Nat.add_zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("describe")
        .expect("Nat.add_zero present");
    assert!(
        row.type_signature.is_some(),
        "expr_to_string_raw should yield a type for Nat.add_zero"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn process_module_projects_real_file_with_header() {
    use lean_rs_host::host::process::ProcessModuleOutcome;

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root.clone(), pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");

    // The same file that returned 0 tactics through `process_file` (because
    // it carries an `import Lean` header) must now project a populated
    // info-tree through `process_module`.
    let source = std::fs::read_to_string(root.join("LeanRsFixture/SourceRanges.lean")).expect("read fixture");
    let outcome = host.process_module(source).await.expect("process_module");

    match outcome {
        ProcessModuleOutcome::Ok { file, imports } | ProcessModuleOutcome::MissingImports { file, imports, .. } => {
            assert!(!file.tactics.is_empty(), "fixture file must record at least one tactic");
            assert!(!file.terms.is_empty(), "fixture file must record at least one term");
            assert!(imports.iter().any(|m| m == "Lean"), "header must include `import Lean`");
        }
        ProcessModuleOutcome::HeaderParseFailed { diagnostics } => {
            panic!("fixture file header should parse; got {diagnostics:?}");
        }
        ProcessModuleOutcome::Unsupported => {
            panic!("fixture capability dylib must export process_module_with_info_tree");
        }
        _ => panic!("unknown ProcessModuleOutcome variant"),
    }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn process_file_projects_tactic_and_term_info() {
    use lean_rs_host::host::process::ProcessFileOutcome;

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    // process_with_info_tree elaborates commands against the open
    // environment; we deliberately omit the `import` header (the
    // environment already has the imports) and provide a self-contained
    // theorem + a `#check` so the projection records tactic, term, and
    // name nodes.
    let source = "\
        theorem fixtureGoal : True := by\n  \
          trivial\n\
        \n\
        #check Nat.succ 0\n";
    let outcome = host
        .process_file(source.to_owned(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("process_file");
    let ProcessFileOutcome::Processed(file) = outcome else {
        panic!("fixture capability dylib must export process_with_info_tree");
    };

    eprintln!(
        "fixture projection: commands={} tactics={} terms={} names={}",
        file.commands.len(),
        file.tactics.len(),
        file.terms.len(),
        file.names.len()
    );
    if let Some(first) = file.tactics.first() {
        eprintln!(
            "first tactic at {}:{}-{}:{} goals_before={:?}",
            first.start_line, first.start_column, first.end_line, first.end_column, first.goals_before
        );
    }
    if let Some(first) = file.terms.first() {
        eprintln!(
            "first term at {}:{}-{}:{} expr={:?} type={:?}",
            first.start_line, first.start_column, first.end_line, first.end_column, first.expr_str, first.type_str
        );
    }
    if let Some(name) = file.names.first() {
        eprintln!(
            "first name {} at {}:{} (binder={})",
            name.name, name.start_line, name.start_column, name.is_binder
        );
    }

    // The fixture must record at least one tactic node (the theorem body)
    // and at least one term node. Asserting via recorded spans is more
    // robust than hardcoding cursor positions against the elaborator's
    // exact recording strategy.
    let tactic = file.tactics.first().expect("at least one tactic node in fixture");
    let hit = file
        .tactic_at(tactic.start_line, tactic.start_column)
        .expect("tactic_at must find a node at its own start position");
    assert_eq!(hit.start_line, tactic.start_line);
    assert!(
        !tactic.goals_before.is_empty(),
        "first tactic should record goals_before"
    );

    let term = file.terms.first().expect("at least one term node in fixture");
    assert!(
        file.term_at(term.start_line, term.start_column).is_some(),
        "term_at must find a node at the first recorded term position"
    );

    // Names: the fixture imports Lean / opens Lean, so at least one
    // reference to a Lean-namespaced symbol must exist.
    assert!(
        !file.names.is_empty(),
        "fixture must record at least one name reference"
    );
}

/// Build a `ToolContext` from a freshly spawned `SessionHost`, sharing one
/// `ProcessedFileCache` so cache-warmth assertions across multiple tool
/// calls in the same test are observable.
fn make_tool_context(
    root: &std::path::Path,
    pkg: String,
    lib: String,
    imports: Vec<String>,
) -> lean_host_mcp::tools::ToolContext {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    let host = SessionHost::spawn(root.to_path_buf(), pkg, lib, imports.clone()).expect("spawn");
    let cache_dir = tempfile::tempdir().expect("tempdir");
    // Leak the tempdir so it outlives the test; the SQLite file is opened
    // for the test's lifetime and the OS will clean /tmp eventually.
    let cache_path = cache_dir.keep();
    let index = lean_host_mcp::DeclarationIndex::open(&cache_path, &root.to_string_lossy()).expect("open index");
    let processed_files = Arc::new(lean_host_mcp::ProcessedFileCache::with_capacity(
        NonZeroUsize::new(16).unwrap(),
    ));
    lean_host_mcp::tools::ToolContext {
        lake_root: host.lake_root().to_owned(),
        default_imports: imports,
        processed_files,
        host,
        index: Arc::new(index),
    }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_returns_clean_file_empty() {
    use lean_host_mcp::tools::position::{FileDiagnosticsRequest, FileDiagnosticsResult, file_diagnostics};

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = make_tool_context(&root, pkg, lib, vec!["LeanRsFixture.Handles".into()]);
    let resp = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file: PathBuf::from("LeanRsFixture/SourceRanges.lean"),
        },
    )
    .await
    .expect("file_diagnostics");
    let FileDiagnosticsResult::Ok { diagnostics, .. } = resp.result else {
        panic!("expected Ok variant, got something else");
    };
    assert!(
        diagnostics.iter().all(|d| d.severity != "Error"),
        "clean fixture should record no error-severity diagnostics; got {diagnostics:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_returns_real_errors() {
    use std::io::Write;

    use lean_host_mcp::tools::position::{FileDiagnosticsRequest, FileDiagnosticsResult, file_diagnostics};

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = make_tool_context(&root, pkg, lib, vec!["LeanRsFixture.Handles".into()]);

    // Two-line file so the error's line number is unambiguous.
    let mut tmp = tempfile::NamedTempFile::with_suffix(".lean").expect("tempfile");
    writeln!(tmp, "-- broken file").unwrap();
    writeln!(tmp, "theorem broken : 1 + 1 = 3 := rfl").unwrap();
    tmp.flush().unwrap();

    let resp = file_diagnostics(
        &ctx,
        FileDiagnosticsRequest {
            file: tmp.path().to_path_buf(),
        },
    )
    .await
    .expect("file_diagnostics");
    let FileDiagnosticsResult::Ok { diagnostics, .. } = resp.result else {
        panic!("expected Ok variant with diagnostics; got something else");
    };
    let error = diagnostics
        .iter()
        .find(|d| d.severity == "Error")
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

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = make_tool_context(&root, pkg, lib, vec!["LeanRsFixture.Handles".into()]);
    let file = PathBuf::from("LeanRsFixture/SourceRanges.lean");

    // Cold: must record the cache-and-process hint.
    let cold = file_diagnostics(&ctx, FileDiagnosticsRequest { file: file.clone() })
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
    let warm = file_diagnostics(&ctx, FileDiagnosticsRequest { file })
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
    use lean_host_mcp::{DeclarationIndex, fingerprint_lake_project};

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let host = SessionHost::spawn(root.clone(), pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let index = DeclarationIndex::open(cache_dir.path(), &root.to_string_lossy()).expect("open index");
    let fp = fingerprint_lake_project(&root).expect("fingerprint");
    let n = index
        .rebuild(&host, vec!["LeanRsFixture.Handles".into()], fp)
        .await
        .expect("rebuild");
    assert!(n > 100, "prelude should expose more than 100 names; got {n}");
    let hits = index.search_theorems("add_zero", 50).expect("search");
    assert!(
        hits.iter().any(|d| d.name == "Nat.add_zero"),
        "Nat.add_zero must be reachable through search_theorems"
    );
}

#[test]
fn envelope_serialises() {
    use lean_host_mcp::{Freshness, Response};
    let r = Response::ok(
        serde_json::json!({"foo": 1}),
        Freshness {
            lake_root: "/tmp/x".into(),
            imports: vec!["A.B".into()],
            session_id: "abc".into(),
            lean_toolchain: "leanprover/lean4:v4.29.1".into(),
        },
    );
    let s = serde_json::to_string(&r).unwrap();
    assert!(s.contains("\"foo\":1"));
    assert!(s.contains("\"lake_root\":\"/tmp/x\""));
    assert!(!s.contains("\"warnings\""), "empty warnings should be omitted");
}
