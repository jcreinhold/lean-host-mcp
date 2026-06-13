//! Opt-in end-to-end tests for the model-facing proof-agent surface.
//!
//! These tests intentionally use only the public six-tool workflow. Raw
//! term/meta probes and `lean_query` are not part of the release surface.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening,
    clippy::indexing_slicing
)]

use std::fs;
use std::path::{Path, PathBuf};

use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::tools::position::{
    FindReferencesRequest, FindReferencesResult, ProofPositionSelector, ProofStateRequest, ProofStateResult,
    ReferenceScope, find_references, proof_state,
};
use lean_host_mcp::tools::proof_action::{
    TryProofStepRequest, VerifyDeclarationRequest, try_proof_step, verify_declaration,
};
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
use lean_host_mcp::tools::semantic::{
    SemanticResponse, SemanticToolRequest, lean_context, lean_lookup, lean_trial, lean_verify,
};
use lean_host_mcp::tools::{TelemetryVerbosity, ToolConfig, ToolContext};
use lean_host_mcp::{
    BrokerConfig, DeclarationInspectionResult, DeclarationVerificationResult, ProjectBroker, ProofAttemptResult,
};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn open_ctx(root: &Path) -> ToolContext {
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.to_path_buf()),
        cwd: root.to_path_buf(),
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
        semantic_permits: BrokerConfig::default_semantic_permits(),
        semantic_waiters: BrokerConfig::default_semantic_waiters(),
        semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
        semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
    });
    ToolContext {
        broker,
        // These tests assert on worker-query telemetry (cache hit/miss, runtime
        // facts), which is gated behind `full` verbosity.
        config: ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    }
}

fn proof_actions_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofActions.lean")
}

fn proof_agent_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofAgent.lean")
}

fn semantic_request(kind: &str, args: serde_json::Value) -> SemanticToolRequest {
    let serde_json::Value::Object(map) = args else {
        panic!("semantic request args must be an object");
    };
    SemanticToolRequest {
        kind: Some(kind.to_owned()),
        args: map.into_iter().collect(),
    }
}

fn semantic_data(response: SemanticResponse<serde_json::Value>) -> serde_json::Value {
    assert!(
        response
            .errors
            .iter()
            .all(|issue| issue.severity.as_deref() != Some("error")),
        "semantic response should not carry error-severity issues: {:?}",
        response.errors
    );
    response.data.expect("semantic data")
}

#[test]
fn request_schemas_are_declaration_centric() {
    let proof: ProofStateRequest =
        serde_json::from_str(r#"{"file":"A.lean","declaration":"A.t","proof_position":{"kind":"index","index":1}}"#)
            .unwrap();
    assert_eq!(proof.declaration, "A.t");

    let attempt: TryProofStepRequest =
        serde_json::from_str(r#"{"file":"A.lean","declaration":"A.t","snippet":"trivial"}"#).unwrap();
    assert_eq!(attempt.declaration, "A.t");

    let value: serde_json::Value =
        serde_json::from_str(r#"{"file":"A.lean","line":4,"column":2,"snippet":"trivial"}"#).unwrap();
    assert!(
        serde_json::from_value::<TryProofStepRequest>(value).is_err(),
        "proof actions must not accept coordinate-only anchors"
    );
}

/// The default proof position is the pristine entry goal: `proof_state` shows
/// it (before == after), a default `try_proof_step` from-scratch block closes
/// the goal against it, and the same block at the post-first-tactic position
/// (`index: 0`) traps — reproducing the `le_saturatedClosure` symptom on the
/// `entryBinderTheorem` fixture.
#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn default_position_is_pristine_entry_and_closes_from_scratch_blocks() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let decl = "LeanRsFixture.ProofActions.entryBinderTheorem".to_owned();
    let from_scratch = "intro p hp; exact hp".to_owned();

    // proof_state at the default shows the pristine entry goal: nothing has run,
    // so before == after, and it is the declaration's opening goal.
    let proof = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: decl.clone(),
            proof_position: ProofPositionSelector::default(),
            project: None,
        },
    )
    .await
    .expect("proof_state");
    let ProofStateResult::Context {
        goals_before,
        goals_after,
        ..
    } = proof.result.expect("proof result")
    else {
        panic!("expected proof context");
    };
    assert_eq!(
        goals_before, goals_after,
        "at the entry no tactic has run, so before == after: {goals_before:?} / {goals_after:?}"
    );
    assert!(
        goals_before.iter().any(|goal| goal.contains("p → p")),
        "entry goal should be the pristine declaration goal: {goals_before:?}"
    );

    // try_proof_step at the default splices before the first tactic, so the
    // from-scratch block elaborates against the pristine goal and closes it.
    let closed = try_proof_step(
        &ctx,
        TryProofStepRequest {
            file: proof_actions_file(),
            declaration: decl.clone(),
            proof_position: ProofPositionSelector::default(),
            project: None,
            snippet: Some(from_scratch.clone()),
            snippets: Vec::new(),
        },
    )
    .await
    .expect("default try_proof_step");
    let ProofAttemptResult::Ok { result, .. } = closed.result.expect("default attempt result") else {
        panic!("expected ok envelope");
    };
    assert_eq!(
        result.candidates[0].status, "closed",
        "from-scratch block must close the goal at the pristine entry default: {:?}",
        result.candidates[0]
    );

    // The same block at the post-first-tactic position (`index: 0`) re-introduces
    // binders already in scope and fails — and the response carries the cue.
    let trapped = try_proof_step(
        &ctx,
        TryProofStepRequest {
            file: proof_actions_file(),
            declaration: decl,
            proof_position: ProofPositionSelector::Index { index: 0 },
            project: None,
            snippet: Some(from_scratch),
            snippets: Vec::new(),
        },
    )
    .await
    .expect("index:0 try_proof_step");
    let cue_warnings = trapped.warnings.clone();
    let cue_next = trapped.next_actions.clone();
    let ProofAttemptResult::Ok { result, .. } = trapped.result.expect("index:0 attempt result") else {
        panic!("expected ok envelope");
    };
    assert_ne!(
        result.candidates[0].status, "closed",
        "a from-scratch block at index:0 must not close (binders already introduced): {:?}",
        result.candidates[0]
    );
    assert!(
        cue_warnings.iter().any(|w| w.contains("already in scope"))
            && cue_next.iter().any(|a| a.contains("pristine entry")),
        "index:0 binder-reintroduction failure should surface the entry cue: warnings={cue_warnings:?} next={cue_next:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn inspect_proof_state_try_verify_and_references() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let inspected = inspect_declaration(
        &ctx,
        InspectDeclarationRequest {
            name: "LeanRsFixture.ProofActions.closedTheorem".to_owned(),
            file: Some(proof_actions_file()),
            imports: Vec::new(),
            project: None,
            fields: InspectDeclarationFields::default(),
            raw_statement: false,
        },
    )
    .await
    .expect("inspect declaration");
    assert!(matches!(
        inspected.result.expect("inspect result"),
        DeclarationInspectionResult::Found { .. }
    ));

    let proof = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::default(),
            project: None,
        },
    )
    .await
    .expect("proof_state");
    let ProofStateResult::Context {
        declaration_name,
        goals_after,
        query_facts,
        ..
    } = proof.result.expect("proof result")
    else {
        panic!("expected proof context");
    };
    assert_eq!(
        declaration_name.as_deref(),
        Some("LeanRsFixture.ProofActions.stepTheorem")
    );
    assert!(
        goals_after.len() <= 1,
        "proof state projection should be bounded and stable"
    );
    assert_eq!(
        query_facts.expect("query_facts under full verbosity").cache_status,
        "miss"
    );

    let warm = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::default(),
            project: None,
        },
    )
    .await
    .expect("warm proof_state");
    let ProofStateResult::Context { query_facts, .. } = warm.result.expect("warm proof result") else {
        panic!("expected warm proof context");
    };
    assert_eq!(
        query_facts.expect("query_facts under full verbosity").cache_status,
        "hit"
    );

    let before = fs::read(root.join(proof_actions_file())).expect("fixture source before");
    let bad = try_proof_step(
        &ctx,
        TryProofStepRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::default(),
            project: None,
            snippet: Some("exact definitely_missing_identifier".to_owned()),
            snippets: Vec::new(),
        },
    )
    .await
    .expect("bad proof attempt");
    let ProofAttemptResult::Ok { result, .. } = bad.result.expect("proof attempt result") else {
        panic!("proof attempt should return ok envelope");
    };
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].status, "failed");
    assert!(
        result.candidates[0]
            .diagnostics
            .diagnostics
            .iter()
            .any(|d| d.message.contains("definitely_missing_identifier")),
        "bad candidate should report local unknown identifier"
    );
    let after = fs::read(root.join(proof_actions_file())).expect("fixture source after");
    assert_eq!(before, after, "try_proof_step must not mutate source files");

    let verified = verify_declaration(
        &ctx,
        VerifyDeclarationRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.closedTheorem".to_owned(),
            project: None,
            allow_sorry: false,
            report_axioms: true,
        },
    )
    .await
    .expect("verify closed theorem");
    assert!(matches!(
        verified.result.expect("verification result"),
        DeclarationVerificationResult::Ok {
            verification_status,
            ..
        } if verification_status == "verified"
    ));

    let sorry = verify_declaration(
        &ctx,
        VerifyDeclarationRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.sorryTheorem".to_owned(),
            project: None,
            allow_sorry: false,
            report_axioms: true,
        },
    )
    .await
    .expect("verify sorry theorem");
    assert!(matches!(
        sorry.result.expect("sorry verification result"),
        DeclarationVerificationResult::Ok {
            verification_status,
            facts,
            ..
        } if verification_status == "has_sorry" && facts.contains_sorry
    ));

    let refs = find_references(
        &ctx,
        FindReferencesRequest {
            name: "LeanRsFixture.ProofActions.closedTheorem".to_owned(),
            scope: ReferenceScope::File,
            file: Some(proof_actions_file()),
            files: Vec::new(),
            limit: Some(10),
            project: None,
        },
    )
    .await
    .expect("find references");
    let FindReferencesResult::Ok { references, .. } = refs.result.expect("references result") else {
        panic!("references should succeed");
    };
    assert!(
        references.iter().any(|reference| reference.kind == "def"),
        "semantic reference lookup should include the declaration site"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn semantic_surface_ports_existing_shipped_behaviors() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let inspected = semantic_data(
        lean_lookup(
            &ctx,
            semantic_request(
                "declaration",
                serde_json::json!({
                    "name": "LeanRsFixture.ProofActions.closedTheorem",
                    "file": "LeanRsFixture/ProofActions.lean"
                }),
            ),
        )
        .await
        .expect("lean_lookup declaration"),
    );
    assert_eq!(
        inspected.pointer("/status").and_then(serde_json::Value::as_str),
        Some("found")
    );

    let proof = semantic_data(
        lean_context(
            &ctx,
            semantic_request(
                "proof_position",
                serde_json::json!({
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.stepTheorem"
                }),
            ),
        )
        .await
        .expect("lean_context proof_position"),
    );
    assert_eq!(
        proof.pointer("/status").and_then(serde_json::Value::as_str),
        Some("context")
    );

    let search = semantic_data(
        lean_lookup(
            &ctx,
            semantic_request(
                "proof_search",
                serde_json::json!({
                    "file": "LeanRsFixture/ProofAgent.lean",
                    "declaration": "LeanRsFixture.ProofAgent.miniRatDenominatorStep",
                    "limit": 5
                }),
            ),
        )
        .await
        .expect("lean_lookup proof_search"),
    );
    assert!(
        search
            .pointer("/candidates")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|candidates| !candidates.is_empty()),
        "proof search should return candidates: {search:?}"
    );

    let before = fs::read(root.join(proof_actions_file())).expect("fixture source before");
    let attempt = semantic_data(
        lean_trial(
            &ctx,
            semantic_request(
                "proof_step",
                serde_json::json!({
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.stepTheorem",
                    "snippet": "trivial"
                }),
            ),
        )
        .await
        .expect("lean_trial proof_step"),
    );
    assert_eq!(
        attempt.pointer("/status").and_then(serde_json::Value::as_str),
        Some("ok")
    );
    let after = fs::read(root.join(proof_actions_file())).expect("fixture source after");
    assert_eq!(before, after, "lean_trial must not mutate source files");

    let verified = semantic_data(
        lean_verify(
            &ctx,
            semantic_request(
                "explicit",
                serde_json::json!({
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declaration": "LeanRsFixture.ProofActions.closedTheorem",
                    "report_axioms": true
                }),
            ),
        )
        .await
        .expect("lean_verify explicit"),
    );
    assert_eq!(
        verified
            .pointer("/verification_status")
            .and_then(serde_json::Value::as_str),
        Some("verified")
    );

    let refs = semantic_data(
        lean_lookup(
            &ctx,
            semantic_request(
                "references",
                serde_json::json!({
                    "name": "LeanRsFixture.ProofActions.closedTheorem",
                    "scope": "file",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "limit": 10
                }),
            ),
        )
        .await
        .expect("lean_lookup references"),
    );
    assert_eq!(refs.pointer("/status").and_then(serde_json::Value::as_str), Some("ok"));
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn find_references_project_scope_reads_index_with_cross_module_hits() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    // Project scope reads the on-disk `.ilean` index (build-fresh), not the
    // worker — so a name defined in one module and used in another comes back
    // whole, with no per-file elaboration.
    let started = std::time::Instant::now();
    let refs = find_references(
        &ctx,
        FindReferencesRequest {
            name: "LeanRsFixture.ProofSearchFacts.MiniRat".to_owned(),
            scope: ReferenceScope::Project,
            file: None,
            files: Vec::new(),
            limit: Some(1000),
            project: None,
        },
    )
    .await
    .expect("find references (project)");
    let elapsed = started.elapsed();

    let FindReferencesResult::Ok {
        references,
        files_scanned,
        ..
    } = refs.result.expect("references result")
    else {
        panic!("project references should succeed");
    };

    // The whole project's `.ilean` modules were indexed, not a single file.
    assert!(
        files_scanned > 1,
        "project scope should index multiple modules, got {files_scanned}"
    );

    // The definition site, with exact coordinates carried from the index. The
    // `.ilean` records `MiniRat` at 0-based `[2,10,2,17]`; the wire form is
    // 1-based on both axes, so this pins the index→wire conversion.
    let def = references
        .iter()
        .find(|reference| reference.kind == "def")
        .expect("definition hit");
    assert!(
        def.file.ends_with("LeanRsFixture/ProofSearchFacts.lean"),
        "def should live in the defining module, got {}",
        def.file
    );
    assert_eq!(
        (def.line, def.column, def.end_line, def.end_column),
        (3, 11, 3, 18),
        "definition coordinates should match the index, converted to 1-based"
    );

    // Cross-module usages: the defining module and a separate consumer module.
    assert!(
        references
            .iter()
            .any(|r| r.kind == "ref" && r.file.ends_with("LeanRsFixture/ProofSearchFacts.lean")),
        "expected a usage in the defining module"
    );
    assert!(
        references
            .iter()
            .any(|r| r.kind == "ref" && r.file.ends_with("LeanRsFixture/ProofAgent.lean")),
        "expected a cross-module usage in ProofAgent"
    );

    // The index read involves no per-file elaboration, so it returns promptly.
    // Generous bound: robust against a cold worker spawn for the freshness
    // snapshot, while still catching a regression to the per-file worker sweep.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "index-backed project scope should be prompt, took {elapsed:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn find_references_project_scope_unbuilt_degrades_to_needs_build() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };

    // Copy the fixture, then drop its reference index so the project reads as
    // "never built". The honest verdict is a `needs_build` warning, not an empty
    // "no references" answer.
    let tmp = tempfile::tempdir().expect("tempdir");
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(format!("{}/.", root.display()))
        .arg(tmp.path())
        .status()
        .expect("copy fixture");
    assert!(status.success(), "cp -R fixture failed");
    let build_index = tmp.path().join(".lake/build/lib/lean");
    if build_index.is_dir() {
        fs::remove_dir_all(&build_index).expect("remove build index");
    }

    let ctx = open_ctx(tmp.path());
    let refs = find_references(
        &ctx,
        FindReferencesRequest {
            name: "LeanRsFixture.ProofSearchFacts.MiniRat".to_owned(),
            scope: ReferenceScope::Project,
            file: None,
            files: Vec::new(),
            limit: Some(1000),
            project: None,
        },
    )
    .await
    .expect("find references (unbuilt)");

    let warnings = refs.warnings.clone();
    let FindReferencesResult::Ok { references, .. } = refs.result.expect("references result") else {
        panic!("unbuilt project should still return an Ok envelope");
    };
    assert!(
        references.is_empty(),
        "an unbuilt project must not invent references, got {references:?}"
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("lake build")),
        "unbuilt project should ride a needs_build/`lake build` warning, got {warnings:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn search_for_proof_prefers_relevant_fixture_lemmas() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let manifest = fs::read_to_string(root.join("lake-manifest.json")).expect("fixture manifest");
    assert!(
        !manifest.contains("lean-semantic-search") && !manifest.contains("LeanSemanticSearch"),
        "fixture must prove zero consumer semantic-search setup"
    );
    let ctx = open_ctx(&root);

    let response = search_for_proof(
        &ctx,
        SearchForProofRequest {
            file: Some(proof_agent_file()),
            declaration: Some("LeanRsFixture.ProofAgent.miniRatDenominatorStep".to_owned()),
            proof_position: ProofPositionSelector::default(),
            goal: None,
            type_text: None,
            imports: Vec::new(),
            mode: Some(ProofSearchMode::NextStep),
            limit: Some(10),
            project: None,
        },
    )
    .await
    .expect("search_for_proof");
    let telemetry = response.telemetry.as_ref().expect("full telemetry");
    assert!(
        telemetry
            .imports
            .iter()
            .all(|import| import != "LeanSemanticSearch.Capability"),
        "semantic capability module must not leak into telemetry imports: {:?}",
        telemetry.imports
    );
    assert!(
        response.warnings.iter().all(|warning| {
            !warning.contains("semantic capability unavailable for this project")
                && !warning.contains("LeanSemanticSearch is not available")
                && !warning.contains("declare")
                && !warning.contains("import LeanSemanticSearch")
        }),
        "warnings must not suggest consumer semantic-search setup: {:?}",
        response.warnings
    );
    assert!(
        response
            .result
            .as_ref()
            .expect("search result")
            .candidates
            .iter()
            .any(|candidate| {
                candidate.name.contains("Rat") && (candidate.name.contains("num") || candidate.name.contains("den"))
            }),
        "fixture arithmetic search should surface Rat num/den structure above generic noise: {:?}",
        response.result.as_ref().expect("search result").candidates
    );
    assert!(
        response
            .result
            .as_ref()
            .expect("search result")
            .candidates
            .iter()
            .any(
                |candidate| candidate.match_reason.contains("semantic:role_conclusion_const")
                    || candidate.match_reason.contains("semantic:conclusion_fingerprint")
                    || candidate.match_reason.contains("semantic:statement_fingerprint")
                    || candidate.match_reason.contains("semantic:safe_permutation_fingerprint")
                    || candidate.match_reason.contains("semantic:connective_fingerprint")
            ),
        "fixture search should include stable semantic evidence; envelope_warnings={:?}; result_warnings={:?}; diagnostics={:?}; candidates={:?}",
        response.warnings,
        response.result.as_ref().expect("search result").warnings,
        response.result.as_ref().expect("search result").diagnostics,
        response.result.as_ref().expect("search result").candidates
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn concurrent_semantic_tools_complete_with_runtime_facts() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let proof = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::default(),
            project: None,
        },
    );
    let inspect = inspect_declaration(
        &ctx,
        InspectDeclarationRequest {
            name: "Nat.add_zero".to_owned(),
            file: Some(proof_actions_file()),
            imports: Vec::new(),
            project: None,
            fields: InspectDeclarationFields::default(),
            raw_statement: false,
        },
    );
    let verify = verify_declaration(
        &ctx,
        VerifyDeclarationRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.closedTheorem".to_owned(),
            project: None,
            allow_sorry: false,
            report_axioms: false,
        },
    );

    let (proof, inspect, verify) = tokio::join!(proof, inspect, verify);
    let proof = proof.expect("proof_state should complete");
    let inspect = inspect.expect("inspect_declaration should complete");
    let verify = verify.expect("verify_declaration should complete");

    assert!(proof.runtime().is_some(), "proof_state should include runtime facts");
    assert!(
        inspect
            .runtime()
            .is_some_and(|runtime| runtime.queue_wait_millis > 0 || runtime.admission_wait_millis > 0),
        "parallel calls should report queue or admission wait metadata"
    );
    assert!(
        verify.runtime().is_some(),
        "verify_declaration should include runtime facts"
    );
}
