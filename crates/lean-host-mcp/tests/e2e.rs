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

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::lean::{
    ElaborateRequest, ElaborateResult, HoverByNameRequest, HoverByNameResult, InferTypeRequest, elaborate,
    hover_by_name, infer_type,
};
use lean_host_mcp::tools::position::{
    DiagnosticsBlock, LeanQueryProjection, LeanQueryRequest, LeanQueryResult, LeanQuerySelector, ModuleQueryFacts,
    ProofStateRequest, ProofStateResult, TypeAtProjection, lean_query, proof_state,
};
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
use lean_host_mcp::{BrokerConfig, ProjectBroker, Response, ServerError};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn mathlib_fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_MATHLIB_FIXTURE")
        .ok()
        .map(PathBuf::from)
}

fn module_syntax_fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_MODULE_SYNTAX_FIXTURE")
        .ok()
        .map(PathBuf::from)
}

fn kanproofs_fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_KANPROOFS_FIXTURE")
        .ok()
        .map(PathBuf::from)
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

fn copy_dir_all(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination dir");
    for entry in fs::read_dir(from).expect("read source dir") {
        let entry = entry.expect("read source entry");
        let file_type = entry.file_type().expect("entry file type");
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&src, &dst);
        } else if file_type.is_file() {
            fs::copy(&src, &dst).expect("copy fixture file");
        } else if file_type.is_symlink() {
            let target = fs::read_link(&src).expect("read symlink");
            std::os::unix::fs::symlink(target, &dst).expect("copy symlink");
        }
    }
}

fn find_module_syntax_file(root: &Path) -> Option<PathBuf> {
    const SKIP_DIRS: &[&str] = &[".git", ".lake", "target"];

    fn visit(dir: &Path, skip_dirs: &[&str]) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir() {
                let name = entry.file_name();
                if skip_dirs.iter().any(|skip| name == *skip) {
                    continue;
                }
                if let Some(found) = visit(&path, skip_dirs) {
                    return Some(found);
                }
            } else if file_type.is_file()
                && path.extension().is_some_and(|ext| ext == "lean")
                && fs::read_to_string(&path).is_ok_and(|source| {
                    source.lines().any(|line| line.trim() == "module")
                        && source.lines().any(|line| line.trim_start().starts_with("import all "))
                })
            {
                return Some(path);
            }
        }
        None
    }

    visit(root, SKIP_DIRS)
}

enum DiagnosticsOutcome {
    Elaborated(DiagnosticsBlock),
    HeaderParseFailed,
    Unsupported,
}

async fn query_diagnostics(ctx: &ToolContext, file: PathBuf) -> lean_host_mcp::Result<Response<LeanQueryResult>> {
    lean_query(
        ctx,
        LeanQueryRequest {
            file,
            selectors: vec![LeanQuerySelector::Diagnostics {
                id: "diagnostics".to_owned(),
            }],
            project: None,
        },
    )
    .await
}

fn diagnostics_outcome(result: LeanQueryResult) -> DiagnosticsOutcome {
    match result {
        LeanQueryResult::Results { mut items, .. } => {
            let item = items.remove("diagnostics").expect("diagnostics selector result");
            let lean_host_mcp::tools::position::LeanQueryItem::Ok {
                result: LeanQueryProjection::Diagnostics(block),
            } = item
            else {
                panic!("diagnostics selector must return diagnostics");
            };
            DiagnosticsOutcome::Elaborated(block)
        }
        LeanQueryResult::HeaderParseFailed { .. } => DiagnosticsOutcome::HeaderParseFailed,
        LeanQueryResult::Unsupported => DiagnosticsOutcome::Unsupported,
        LeanQueryResult::InvalidSelectors { message } => panic!("unexpected invalid diagnostics query: {message}"),
    }
}

fn query_facts(result: &LeanQueryResult) -> &ModuleQueryFacts {
    match result {
        LeanQueryResult::Results { query_facts, .. } | LeanQueryResult::HeaderParseFailed { query_facts, .. } => {
            query_facts
        }
        LeanQueryResult::InvalidSelectors { .. } | LeanQueryResult::Unsupported => {
            panic!("expected query facts on processed query result")
        }
    }
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
            max_type_bytes: None,
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
fn search_declarations_request_round_trips() {
    use lean_host_mcp::tools::lean::SearchDeclarationsRequest;

    let minimal: SearchDeclarationsRequest = serde_json::from_str(r#"{"query":"add_zero"}"#).unwrap();
    assert_eq!(minimal.query, "add_zero");
    assert!(minimal.imports.is_empty());
    assert!(minimal.limit.is_none());
    assert!(minimal.include_source);

    let full: SearchDeclarationsRequest =
        serde_json::from_str(r#"{"query":"map","kind":"theorem","imports":["List"],"limit":25}"#).unwrap();
    assert_eq!(full.limit, Some(25));
    assert_eq!(full.kind.as_deref(), Some("theorem"));
    assert_eq!(full.imports, vec!["List".to_owned()]);
}

#[test]
fn search_for_proof_request_round_trips() {
    let cursor: SearchForProofRequest =
        serde_json::from_str(r#"{"file":"A.lean","line":4,"column":2,"mode":"apply","limit":200}"#).unwrap();
    assert_eq!(cursor.file, Some(PathBuf::from("A.lean")));
    assert_eq!(cursor.line, Some(4));
    assert_eq!(cursor.column, Some(2));
    assert_eq!(cursor.mode, Some(ProofSearchMode::Apply));
    assert_eq!(cursor.limit, Some(200));

    let explicit: SearchForProofRequest = serde_json::from_str(r#"{"goal":"⊢ True"}"#).unwrap();
    assert_eq!(explicit.goal.as_deref(), Some("⊢ True"));
    assert!(explicit.file.is_none());
    assert!(explicit.mode.is_none());
}

#[test]
fn type_of_name_request_accepts_byte_cap() {
    use lean_host_mcp::tools::lean::TypeOfNameRequest;

    let nat: TypeOfNameRequest = serde_json::from_str(r#"{"name":"Nat.add_zero","max_type_bytes":4096}"#).unwrap();
    assert_eq!(nat.name, "Nat.add_zero");
    assert_eq!(nat.max_type_bytes, Some(4096));

    let none: TypeOfNameRequest = serde_json::from_str(r#"{"name":"Nat.add_zero"}"#).unwrap();
    assert!(none.max_type_bytes.is_none());
}

#[test]
fn position_requests_round_trip() {
    use lean_host_mcp::tools::position::{
        LeanQueryRequest, ProofStateRequest, ReferencesInFileRequest, ReferencesInProjectRequest,
    };

    let g: ProofStateRequest = serde_json::from_str(r#"{"file":"Foo/Bar.lean","line":7,"column":3}"#).unwrap();
    assert_eq!(g.line, 7);
    assert_eq!(g.column, 3);

    let q: LeanQueryRequest =
        serde_json::from_str(r#"{"file":"X.lean","selectors":[{"selector":"type_at","id":"t","line":1,"column":1}]}"#)
            .unwrap();
    assert_eq!(q.selectors.len(), 1);

    let r_file: ReferencesInFileRequest = serde_json::from_str(r#"{"file":"A.lean","name":"Nat.add"}"#).unwrap();
    assert_eq!(r_file.file, PathBuf::from("A.lean"));

    let r_project: ReferencesInProjectRequest =
        serde_json::from_str(r#"{"name":"Nat.add","files":["A.lean","B.lean"],"limit":25}"#).unwrap();
    assert_eq!(r_project.files.len(), 2);
    assert_eq!(r_project.limit, Some(25));
}

#[test]
fn references_result_skips_empty_fields() {
    use lean_host_mcp::tools::position::ReferencesInProjectResult;

    let empty = ReferencesInProjectResult {
        references: Vec::new(),
        truncated: false,
        files_scanned: 0,
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

    let with_flags = ReferencesInProjectResult {
        references: Vec::new(),
        truncated: true,
        files_scanned: 1,
        unsupported_files: vec!["A.lean".into()],
        header_parse_failed_files: Vec::new(),
        missing_imports_files: Vec::new(),
    };
    let s = serde_json::to_string(&with_flags).unwrap();
    assert!(s.contains("\"truncated\":true"));
    assert!(s.contains("\"unsupported_files\":[\"A.lean\"]"));
}

#[test]
fn proof_state_result_serialises_status_tag() {
    use lean_host_mcp::tools::position::ProofStateResult;

    let s = serde_json::to_string(&ProofStateResult::Unsupported).unwrap();
    assert_eq!(s, r#"{"status":"unsupported"}"#);
}

#[test]
fn lean_query_diagnostics_request_round_trips() {
    use lean_host_mcp::tools::position::LeanQueryRequest;

    let r: LeanQueryRequest =
        serde_json::from_str(r#"{"file":"Foo/Bar.lean","selectors":[{"selector":"diagnostics","id":"d"}]}"#).unwrap();
    assert_eq!(r.file, PathBuf::from("Foo/Bar.lean"));
}

#[test]
fn lean_query_result_serialises_status_tag() {
    use lean_host_mcp::tools::position::{DiagnosticSummary, LeanQueryResult, ModuleQueryFacts, ModuleQueryTimings};

    let s = serde_json::to_string(&LeanQueryResult::Unsupported).unwrap();
    assert_eq!(s, r#"{"status":"unsupported"}"#);

    let s = serde_json::to_string(&LeanQueryResult::HeaderParseFailed {
        summary: DiagnosticSummary::default(),
        diagnostics: Vec::new(),
        truncated: false,
        query_facts: ModuleQueryFacts {
            cache_status: "miss",
            output_bytes: 0,
            cache_entry_count: None,
            cache_approx_bytes: None,
            timings: ModuleQueryTimings {
                header_import_micros: 0,
                elaboration_micros: 0,
                projection_micros: 0,
                rendering_micros: 0,
            },
        },
    })
    .unwrap();
    assert_eq!(
        s,
        r#"{"status":"header_parse_failed","summary":{"errors":0,"warnings":0,"info":0},"diagnostics":[],"truncated":false,"query_facts":{"cache_status":"miss","output_bytes":0,"timings":{"header_import_micros":0,"elaboration_micros":0,"projection_micros":0,"rendering_micros":0}}}"#
    );
}

#[tokio::test]
#[ignore = "requires a built mathlib-dependent Lake fixture; set LEAN_HOST_MCP_TEST_MATHLIB_FIXTURE to enable"]
async fn mathlib_fixture_uses_transitive_package_search_paths() {
    use std::io::Write as _;

    use lean_host_mcp::tools::lean::{SearchDeclarationsRequest, search_declarations};

    let Some(root) = mathlib_fixture_root() else {
        eprintln!("skipping: LEAN_HOST_MCP_TEST_MATHLIB_FIXTURE not set");
        return;
    };
    let ctx = open_ctx(&root);

    let inferred = infer_type(
        &ctx,
        InferTypeRequest {
            term: "fun (n : Nat) => n + 1".into(),
            imports: vec!["Mathlib".into()],
            project: None,
        },
    )
    .await
    .expect("infer_type with Mathlib import");
    assert_eq!(
        inferred.result.status, "Ok",
        "infer_type must import Mathlib through transitive package paths: {:?}",
        inferred.result
    );
    assert!(
        matches!(inferred.result.rendered.as_deref(), Some("Nat → Nat" | "ℕ → ℕ")),
        "Mathlib import should still infer the Nat function type; got {:?}",
        inferred.result.rendered
    );

    let symbols = search_declarations(
        &ctx,
        SearchDeclarationsRequest {
            query: "pow_left_injective".into(),
            kind: None,
            imports: vec!["Mathlib.Data.Nat.Basic".into()],
            limit: Some(500),
            include_source: true,
            project: None,
        },
    )
    .await
    .expect("search_declarations with Mathlib import");
    assert!(
        symbols.result.declarations.iter().any(|row| {
            row.name.starts_with("Mathlib.")
                || row.source.as_ref().is_some_and(|source| {
                    source.file.contains(".lake/packages/mathlib")
                        || source.file.contains("Mathlib/")
                        || source.file.starts_with("Mathlib.")
                })
        }),
        "pow_left_injective search should include a declaration from mathlib; got {:?}",
        symbols.result.declarations
    );

    let mut file = tempfile::Builder::new()
        .prefix("lean_host_mcp_mathlib_")
        .suffix(".lean")
        .tempfile_in(&root)
        .expect("create temporary Mathlib-importing project file");
    writeln!(file, "import Mathlib").unwrap();
    writeln!(file).unwrap();
    writeln!(file, "example : (0 : Nat) + 1 = 1 := by norm_num").unwrap();
    file.flush().unwrap();

    let diagnostics = query_diagnostics(&ctx, file.path().to_path_buf())
        .await
        .expect("lean_query diagnostics on Mathlib-importing file");
    assert!(
        diagnostics
            .warnings
            .iter()
            .all(|warning| !warning.contains("missing imports") && !warning.contains("open env does not have")),
        "Mathlib imports should resolve without missing-import envelope warnings for {:?}: {:?}",
        file.path(),
        diagnostics.warnings
    );
    let DiagnosticsOutcome::Elaborated(block) = diagnostics_outcome(diagnostics.result) else {
        panic!("lean_query diagnostics must elaborate a Mathlib-importing project file");
    };
    assert_eq!(
        block.summary.errors, 0,
        "Mathlib-importing project file should elaborate cleanly"
    );
}

#[tokio::test]
#[ignore = "requires a built module-syntax Lake fixture; set LEAN_HOST_MCP_TEST_MODULE_SYNTAX_FIXTURE to enable"]
async fn module_syntax_file_diagnostics_elaborates_import_all_header() {
    let Some(root) = module_syntax_fixture_root() else {
        eprintln!("skipping: LEAN_HOST_MCP_TEST_MODULE_SYNTAX_FIXTURE not set");
        return;
    };
    let file = find_module_syntax_file(&root).unwrap_or_else(|| {
        panic!(
            "fixture must contain a .lean file with a standalone `module` line and an `import all` header: {}",
            root.display()
        )
    });
    let ctx = open_ctx(&root);

    let diagnostics = query_diagnostics(&ctx, file.clone()).await.unwrap_or_else(|err| {
        panic!(
            "lean_query diagnostics must not propagate an import-prefix error for {}: {err:?}",
            file.display()
        )
    });
    assert!(
        diagnostics
            .warnings
            .iter()
            .all(|warning| !warning.contains("unknown module prefix 'all'")),
        "module-syntax diagnostics must not warn about `all` as a module prefix for {}: {:?}",
        file.display(),
        diagnostics.warnings
    );
    let DiagnosticsOutcome::Elaborated(_) = diagnostics_outcome(diagnostics.result) else {
        panic!(
            "module-syntax file should elaborate far enough to return diagnostics for {}",
            file.display()
        );
    };
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
            max_type_bytes: None,
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
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let resp = query_diagnostics(&ctx, PathBuf::from("LeanRsFixture/SourceRanges.lean"))
        .await
        .expect("lean_query diagnostics");
    let DiagnosticsOutcome::Elaborated(block) = diagnostics_outcome(resp.result) else {
        panic!("expected Elaborated variant, got something else");
    };
    assert_eq!(
        block.summary.errors, 0,
        "clean fixture should record no error-severity diagnostics; got {:?}",
        block.diagnostics
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_returns_real_errors() {
    use std::io::Write;

    use lean_host_mcp::Severity;

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    // Two-line file so the error's line number is unambiguous.
    let mut tmp = tempfile::NamedTempFile::with_suffix(".lean").expect("tempfile");
    writeln!(tmp, "-- broken file").unwrap();
    writeln!(tmp, "theorem broken : 1 + 1 = 3 := rfl").unwrap();
    tmp.flush().unwrap();

    let resp = query_diagnostics(&ctx, tmp.path().to_path_buf())
        .await
        .expect("lean_query diagnostics");
    let DiagnosticsOutcome::Elaborated(block) = diagnostics_outcome(resp.result) else {
        panic!("expected Elaborated variant with diagnostics; got something else");
    };
    assert!(
        block.summary.errors >= 1,
        "summary.errors must reflect the deliberate failure; got {:?}",
        block.summary
    );
    let error = block
        .diagnostics
        .iter()
        .find(|d| matches!(d.severity, Severity::Error))
        .expect("at least one error-severity diagnostic for `1 + 1 = 3 := rfl`");
    let pos = error.position.as_ref().expect("error diagnostic has a position");
    assert_eq!(pos.line, 2, "error must be reported on the theorem line (got {pos:?})");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn module_query_position_tools_return_bounded_results() {
    use lean_host_mcp::tools::position::{
        LeanQueryItem, ReferencesInFileRequest, ReferencesInProjectRequest, references_in_file, references_in_project,
    };

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let file = PathBuf::from("LeanRsFixture/SourceRanges.lean");

    let type_resp = lean_query(
        &ctx,
        LeanQueryRequest {
            file: file.clone(),
            selectors: vec![LeanQuerySelector::TypeAt {
                id: "type".to_owned(),
                line: 7,
                column: 9,
            }],
            project: None,
        },
    )
    .await
    .expect("lean_query type_at");
    let LeanQueryResult::Results { mut items, .. } = type_resp.result else {
        panic!("expected lean_query results");
    };
    let LeanQueryItem::Ok {
        result: LeanQueryProjection::TypeAt(TypeAtProjection::Term { type_str, .. }),
    } = items.remove("type").expect("type selector result")
    else {
        panic!("expected a term at theorem name");
    };
    assert!(
        !type_str.value.is_empty(),
        "type_at_position should render only the selected term type"
    );

    let goal_resp = proof_state(
        &ctx,
        ProofStateRequest {
            file: file.clone(),
            line: 8,
            column: 3,
            project: None,
        },
    )
    .await
    .expect("proof_state");
    let response_bytes = serde_json::to_vec(&goal_resp)
        .expect("serialize proof_state response")
        .len();
    assert!(
        response_bytes < 64 * 1024,
        "fixture proof_state response should stay under the hard 64 KiB cap, got {response_bytes} bytes"
    );
    let ProofStateResult::Context {
        diagnostics,
        goals_before,
        locals,
        expected_type,
        safe_edit,
        truncated,
        span,
        target_declaration,
        surrounding_declaration,
        query_facts,
        ..
    } = goal_resp.result
    else {
        panic!("expected a tactic context at `trivial`");
    };
    assert_eq!(diagnostics.summary.errors, 0);
    assert!(span.is_some(), "proof_state should include the cursor context span");
    assert!(locals.is_empty(), "fixture tactic context has no locals");
    assert!(
        expected_type.as_ref().is_none_or(|text| !text.value.is_empty()),
        "expected type should be omitted or non-empty"
    );
    assert!(
        safe_edit.is_some(),
        "fixture proof_state should include safe edit metadata"
    );
    assert!(
        target_declaration.is_some(),
        "proof_state should include target declaration status"
    );
    assert!(
        surrounding_declaration.is_some(),
        "proof_state should include surrounding declaration status"
    );
    assert!(!truncated, "small fixture goal should not truncate");
    assert!(
        goals_before.iter().any(|goal| goal.contains("True")),
        "goal context should mention True: {goals_before:?}"
    );
    assert!(
        ["hit", "miss", "rebuilt", "evicted"].contains(&query_facts.cache_status),
        "proof_state should expose worker cache facts: {query_facts:?}"
    );

    let name = "LeanRsFixture.SourceRanges.knownTheorem".to_owned();
    let file_refs = references_in_file(
        &ctx,
        ReferencesInFileRequest {
            file: file.clone(),
            name: name.clone(),
            project: None,
        },
    )
    .await
    .expect("references_in_file");
    assert!(
        file_refs.result.references.iter().any(|hit| hit.kind == "def"),
        "file-local references should include the binder for {name}: {:?}",
        file_refs.result.references
    );

    let project_refs = references_in_project(
        &ctx,
        ReferencesInProjectRequest {
            name,
            files: vec![file],
            limit: Some(1),
            project: None,
        },
    )
    .await
    .expect("references_in_project");
    assert_eq!(project_refs.result.files_scanned, 1);
    assert!(
        project_refs.result.references.len() <= 1,
        "project reference scan must obey the requested limit"
    );
}

#[tokio::test]
#[ignore = "requires built KanProofs; set LEAN_HOST_MCP_TEST_KANPROOFS_FIXTURE to enable"]
async fn kanproofs_basechange_restrict_diagnostics_stays_bounded() {
    let Some(root) = kanproofs_fixture_root() else {
        eprintln!("skipping: LEAN_HOST_MCP_TEST_KANPROOFS_FIXTURE not set");
        return;
    };
    let file = std::env::var("LEAN_HOST_MCP_TEST_KANPROOFS_FILE").map_or_else(
        |_| PathBuf::from("KanProofs/AlgebraicGeometry/Sites/FiniteEtale/Quotient/BaseChange/Restrict.lean"),
        PathBuf::from,
    );
    let ctx = open_ctx(&root);

    let diagnostics = query_diagnostics(&ctx, file.clone()).await.unwrap_or_else(|err| {
        panic!(
            "bounded lean_query diagnostics must not fail for {}: {err:?}",
            file.display()
        )
    });
    assert!(
        diagnostics
            .warnings
            .iter()
            .all(|warning| !warning.contains("worker protocol frame too large")),
        "bounded diagnostics should not surface frame-size warnings: {:?}",
        diagnostics.warnings
    );
    let outcome = diagnostics_outcome(diagnostics.result);
    assert!(
        matches!(
            outcome,
            DiagnosticsOutcome::Elaborated(_) | DiagnosticsOutcome::HeaderParseFailed
        ),
        "KanProofs smoke should return a structured diagnostics result"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn per_call_imports_avoid_broken_project_umbrella_failure() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_root = tmp.path().join("fixture-copy");
    copy_dir_all(&root, &project_root);

    let broken_file = project_root.join("LeanRsFixture/Broken.lean");
    fs::write(&broken_file, "theorem broken : True := sorry_that_doesnt_exist\n").expect("write broken module");

    let broken_target = Command::new("lake")
        .args(["build", "LeanRsFixture.Broken"])
        .current_dir(&project_root)
        .output()
        .expect("run lake build LeanRsFixture.Broken");
    assert!(
        !broken_target.status.success(),
        "broken project-local module must fail when built directly; stdout={}, stderr={}",
        String::from_utf8_lossy(&broken_target.stdout),
        String::from_utf8_lossy(&broken_target.stderr)
    );

    let ctx = open_ctx(&project_root);
    let inferred_without_imports = infer_type(
        &ctx,
        InferTypeRequest {
            term: "fun (n : Nat) => Nat.succ n".into(),
            imports: Vec::new(),
            project: None,
        },
    )
    .await
    .expect("infer_type without project imports");
    assert_eq!(
        inferred_without_imports.result.status, "Ok",
        "infer_type with no caller imports must succeed: {:?}",
        inferred_without_imports.result
    );
    assert!(
        matches!(
            inferred_without_imports.result.rendered.as_deref(),
            Some("Nat → Nat" | "Nat -> Nat")
        ),
        "expected Nat function type with no imports; got {:?}",
        inferred_without_imports.result.rendered
    );
    assert!(
        inferred_without_imports.freshness.imports.is_empty(),
        "freshness imports must reflect the empty request"
    );

    let inferred_with_umbrella = infer_type(
        &ctx,
        InferTypeRequest {
            term: "fun (n : Nat) => Nat.succ n".into(),
            imports: vec!["LeanRsFixture".into()],
            project: None,
        },
    )
    .await
    .expect("infer_type against umbrella import");
    assert_eq!(
        inferred_with_umbrella.result.status, "Ok",
        "the unchanged umbrella must still import successfully: {:?}",
        inferred_with_umbrella.result
    );

    let diagnostics = query_diagnostics(&ctx, PathBuf::from("LeanRsFixture/Broken.lean"))
        .await
        .expect("lean_query diagnostics on broken project-local module");
    let DiagnosticsOutcome::Elaborated(block) = diagnostics_outcome(diagnostics.result) else {
        panic!("expected real diagnostics for broken file");
    };
    assert!(
        block.summary.errors >= 1,
        "broken file must report an error: {:?}",
        block.diagnostics
    );
    assert!(
        block
            .diagnostics
            .iter()
            .any(|d| d.message.contains("sorry_that_doesnt_exist")),
        "diagnostics should name the broken identifier: {:?}",
        block.diagnostics
    );

    let import_err = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Broken".into()],
            project: None,
        },
    )
    .await
    .expect_err("explicitly importing the broken module must fail");
    match &import_err {
        ServerError::Lean(msg) => {
            assert!(
                msg.contains("lean_exception")
                    || msg.contains("LeanRsFixture.Broken")
                    || msg.contains("olean")
                    || msg.contains("sorry_that_doesnt_exist")
                    || msg.contains("unknown identifier"),
                "expected Lean import failure, got: {msg}"
            );
        }
        ServerError::BadProject(msg) => {
            panic!("broken explicit import should not be a bootstrap failure: {msg}")
        }
        ServerError::SessionGone | ServerError::Index(_) | ServerError::Io(_) | ServerError::Internal(_) => {
            panic!("broken explicit import should be a Lean failure, got: {import_err:?}")
        }
    }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn file_diagnostics_cache_warm() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let file = PathBuf::from("LeanRsFixture/SourceRanges.lean");

    // Cold: first worker query for this file/selector shape misses the worker snapshot cache.
    let cold = query_diagnostics(&ctx, file.clone())
        .await
        .expect("cold lean_query diagnostics");
    assert_eq!(query_facts(&cold.result).cache_status, "miss");

    // Repeat: same bounded query, same file contents reaches the worker and reports its cache behavior.
    let warm = query_diagnostics(&ctx, file)
        .await
        .expect("warm lean_query diagnostics");
    let warm_facts = query_facts(&warm.result);
    assert!(
        ["hit", "miss", "rebuilt", "evicted"].contains(&warm_facts.cache_status),
        "worker should report a known cache status, got {:?}",
        warm_facts.cache_status
    );
    assert!(warm_facts.output_bytes > 0, "worker should report output bytes");
    assert!(
        warm.next_actions
            .iter()
            .any(|n| n.contains("worker module snapshot cache status:")),
        "warm file_diagnostics call should report worker cache facts; next_actions={:?}",
        warm.next_actions
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn declaration_search_finds_prelude_theorem_without_index_rebuild() {
    use lean_host_mcp::tools::lean::{SearchDeclarationsRequest, search_declarations};

    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let init_resp = search_declarations(
        &ctx,
        SearchDeclarationsRequest {
            query: "Nat.add".into(),
            kind: None,
            imports: Vec::new(),
            limit: Some(10),
            include_source: true,
            project: None,
        },
    )
    .await
    .expect("search_declarations with no imports");
    assert!(
        init_resp.freshness.imports.is_empty(),
        "freshness imports must reflect the empty request"
    );

    let resp = search_declarations(
        &ctx,
        SearchDeclarationsRequest {
            query: "add_zero".into(),
            kind: Some("theorem".into()),
            imports: vec!["LeanRsFixture.Handles".into()],
            limit: Some(50),
            include_source: true,
            project: None,
        },
    )
    .await
    .expect("search_declarations theorem filter");
    assert_eq!(
        resp.freshness.imports,
        vec!["LeanRsFixture.Handles".to_owned()],
        "freshness imports must reflect the per-call request"
    );
    assert!(
        resp.next_actions
            .iter()
            .all(|hint| !hint.contains("declaration index was rebuilt")),
        "declaration search must not rebuild a SQLite index; next_actions={:?}",
        resp.next_actions
    );
    assert!(
        resp.result.declarations.iter().any(|d| d.name == "Nat.add_zero"),
        "Nat.add_zero must be reachable through bounded declaration search"
    );
    assert!(
        resp.result
            .declarations
            .iter()
            .all(|d| !d.match_reason.is_empty() && d.rank > 0),
        "search v2 rows must expose match reason and rank: {:?}",
        resp.result.declarations
    );
    assert!(
        resp.result.facts.declarations_scanned > 0,
        "search v2 facts should report scan counts: {:?}",
        resp.result.facts
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn search_for_proof_explicit_goal_returns_bounded_candidates() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let resp = search_for_proof(
        &ctx,
        SearchForProofRequest {
            file: None,
            line: None,
            column: None,
            goal: Some("⊢ True".into()),
            type_text: None,
            imports: vec!["LeanRsFixture.SourceRanges".into()],
            mode: Some(ProofSearchMode::Exact),
            limit: Some(5),
            project: None,
        },
    )
    .await
    .expect("search_for_proof explicit goal");

    let response_bytes = serde_json::to_vec(&resp)
        .expect("serialize search_for_proof response")
        .len();
    assert!(
        response_bytes < 64 * 1024,
        "search_for_proof response should stay under hard budget, got {response_bytes}"
    );
    assert_eq!(resp.result.diagnostics.proof_state_status, "explicit_text");
    assert!(resp.result.diagnostics.generated_count > 0);
    assert!(
        resp.result.candidates.iter().any(|candidate| {
            candidate.name == "LeanRsFixture.SourceRanges.knownTheorem" || candidate.name.contains("True")
        }),
        "explicit True goal should retrieve a plausible theorem: {:?}",
        resp.result.candidates
    );
    assert!(
        resp.result
            .candidates
            .iter()
            .all(|candidate| !candidate.match_reason.is_empty()),
        "candidates should include deterministic match reasons"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn search_for_proof_cursor_goal_reports_cache_and_candidates() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let resp = search_for_proof(
        &ctx,
        SearchForProofRequest {
            file: Some(PathBuf::from("LeanRsFixture/SourceRanges.lean")),
            line: Some(8),
            column: Some(3),
            goal: None,
            type_text: None,
            imports: Vec::new(),
            mode: Some(ProofSearchMode::NextStep),
            limit: Some(5),
            project: None,
        },
    )
    .await
    .expect("search_for_proof cursor");

    assert_eq!(resp.result.diagnostics.proof_state_status, "context");
    assert!(
        resp.result.diagnostics.cache_status.is_some(),
        "cursor search should surface proof-state cache status"
    );
    assert!(
        resp.result.diagnostics.returned_count <= 5,
        "limit should cap returned candidates"
    );
    assert!(
        resp.result.candidates.iter().all(|candidate| {
            candidate.source.is_none() || candidate.source.as_ref().is_some_and(|source| !source.file.is_empty())
        }),
        "candidate source ranges, when present, must be bounded metadata only"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn search_for_proof_broad_goal_reports_pruning_without_type_text() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let resp = search_for_proof(
        &ctx,
        SearchForProofRequest {
            file: None,
            line: None,
            column: None,
            goal: Some("⊢ a = b".into()),
            type_text: None,
            imports: vec!["Lean".into()],
            mode: Some(ProofSearchMode::Rewrite),
            limit: Some(3),
            project: None,
        },
    )
    .await
    .expect("search_for_proof broad equality");

    assert_eq!(resp.result.diagnostics.proof_state_status, "explicit_text");
    assert!(resp.result.diagnostics.returned_count <= 3);
    let serialized = serde_json::to_string(&resp).expect("serialize response");
    assert!(
        !serialized.contains("type_signature") && !serialized.contains("statement"),
        "Prompt 40 candidates must not include rendered declaration text: {serialized}"
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
