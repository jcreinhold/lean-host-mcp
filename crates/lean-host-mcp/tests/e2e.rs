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

use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::tools::position::{
    FindReferencesRequest, FindReferencesResult, ProofPositionSelector, ProofStateRequest, ProofStateResult,
    ReferenceScope, find_references, proof_state,
};
use lean_host_mcp::tools::proof_action::{
    TryProofStepRequest, VerifyDeclarationRequest, try_proof_step, verify_declaration,
};
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
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
    });
    ToolContext { broker }
}

fn proof_actions_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofActions.lean")
}

fn proof_agent_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofAgent.lean")
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
            max_field_bytes: Some(512),
            max_total_bytes: Some(2048),
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
    assert_eq!(query_facts.cache_status, "miss");

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
    assert_eq!(query_facts.cache_status, "hit");

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
            max_field_bytes: None,
            max_total_bytes: None,
            heartbeat_limit: None,
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
            max_field_bytes: None,
            max_total_bytes: None,
            heartbeat_limit: None,
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
            max_field_bytes: None,
            max_total_bytes: None,
            heartbeat_limit: None,
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
async fn search_for_proof_prefers_relevant_fixture_lemmas() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
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
            max_field_bytes: Some(512),
            max_total_bytes: Some(2048),
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
            max_field_bytes: None,
            max_total_bytes: None,
            heartbeat_limit: None,
        },
    );

    let (proof, inspect, verify) = tokio::join!(proof, inspect, verify);
    let proof = proof.expect("proof_state should complete");
    let inspect = inspect.expect("inspect_declaration should complete");
    let verify = verify.expect("verify_declaration should complete");

    assert!(proof.runtime.is_some(), "proof_state should include runtime facts");
    assert!(
        inspect
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.queue_wait_millis > 0 || runtime.admission_wait_millis > 0),
        "parallel calls should report queue or admission wait metadata"
    );
    assert!(
        verify.runtime.is_some(),
        "verify_declaration should include runtime facts"
    );
}
