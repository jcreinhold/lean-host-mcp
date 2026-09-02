//! Opt-in end-to-end tests for the model-facing proof-agent surface.
//!
//! These tests intentionally use only the public six-tool workflow. Raw
//! term/meta probes and `lean_query` are not part of the release surface.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening,
    clippy::indexing_slicing,
    // The rebuild tests move an mtime with `SystemTime + Duration`; overflow
    // is not a meaningful failure mode for a test clock bump.
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::{Path, PathBuf};

// Four tests below share one mutable file: the fixture's `Handles.olean`,
// whose mtime the rebuild tests bump to stand in for `lake build`, while the
// residue-budget tests assert across multi-second sleeps that the worker
// serving that same import never recycles. Under the default parallel test
// runner one test's bump lands inside another's stability window and the
// assertions fail with `artifacts_rebuilt`; the named serial key confines
// the exclusion to exactly these four, leaving the rest of the suite
// parallel.
use serial_test::serial;

use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::tools::position::{
    FindReferencesRequest, FindReferencesResult, ProofBoundarySelector, ProofPositionSelector, ProofStateRequest,
    ProofStateResult, ReferenceScope, find_references, proof_state,
};
use lean_host_mcp::tools::proof_action::{
    TryProofStepRequest, VerifyDeclarationRequest, try_proof_step, verify_declaration,
};
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
use lean_host_mcp::tools::semantic::{
    SemanticResponse, SemanticToolRequest, lean_context, lean_lookup, lean_status, lean_trial, lean_verify,
};
use lean_host_mcp::tools::{OutputBudgetOverrides, TelemetryVerbosity, ToolConfig, ToolContext};
use lean_host_mcp::{
    BrokerConfig, CoordinateSpace, DeclarationInspectionResult, DeclarationVerificationResult, ProjectBroker,
    ProjectHint, ProjectRuntimeConfig, ProofAttemptResult, Severity,
};
use lean_rs_worker_parent::{LeanWorkerDeclarationFilter, LeanWorkerDeclarationNameMatch, LeanWorkerDeclarationSearch};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn open_ctx(root: &Path) -> ToolContext {
    open_ctx_with_config(
        root,
        ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    )
}

/// A context whose project actor runs under an explicit residue budget, so a
/// test can reach the residue paths without a Mathlib-scale import. Everything
/// else is the shipped default.
fn open_ctx_with_residue_budget(root: &Path, budget_bytes: u64) -> ToolContext {
    let broker = ProjectBroker::new_with_runtime_config(
        BrokerConfig {
            config_default: None,
            env_default: Some(root.to_path_buf()),
            cwd: root.to_path_buf(),
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: BrokerConfig::default_idle_timeout(),
        },
        ProjectRuntimeConfig::default().with_import_residue_budget_bytes(budget_bytes),
    );
    ToolContext {
        broker,
        config: ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    }
}

fn open_ctx_with_config(root: &Path, config: ToolConfig) -> ToolContext {
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.to_path_buf()),
        cwd: root.to_path_buf(),
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    ToolContext { broker, config }
}

fn proof_actions_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofActions.lean")
}

fn proof_agent_file() -> PathBuf {
    PathBuf::from("LeanRsFixture/ProofAgent.lean")
}

fn copy_dir_recursive(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create destination dir");
    for entry in fs::read_dir(from).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let file_type = entry.file_type().expect("entry type");
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest);
        } else if file_type.is_file() {
            fs::copy(entry.path(), dest).expect("copy file");
        }
    }
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

fn semantic_request_without_kind(args: serde_json::Value) -> SemanticToolRequest {
    let serde_json::Value::Object(map) = args else {
        panic!("semantic request args must be an object");
    };
    SemanticToolRequest {
        kind: None,
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
            include_boundaries: false,
            include_expected_type: false,
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
            retry_tainted_non_positive: false,
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
            retry_tainted_non_positive: false,
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
async fn unresolved_after_text_returns_boundary_candidates_and_retry_selector() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let declaration = "LeanRsFixture.ProofActions.stepTheorem".to_owned();

    let unresolved = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: declaration.clone(),
            proof_position: ProofPositionSelector::AfterText {
                text: "not a complete tactic boundary".to_owned(),
                occurrence: None,
            },
            include_boundaries: true,
            include_expected_type: false,
            project: None,
        },
    )
    .await
    .expect("unresolved after_text proof_state");
    let ProofStateResult::Context {
        unavailable,
        proof_boundaries,
        proof_boundaries_truncated,
        ..
    } = unresolved.result.expect("unresolved proof-state result")
    else {
        panic!("expected context payload");
    };

    assert!(
        unavailable.iter().any(|entry| entry.id == "proof_state"),
        "unresolved selector should be reported as a Lean-domain unavailable selector: {unavailable:?}"
    );
    assert!(
        !proof_boundaries.is_empty(),
        "unresolved selector should return candidate proof boundaries"
    );
    assert!(
        !proof_boundaries_truncated,
        "small fixture should not truncate proof boundaries"
    );
    for pair in proof_boundaries.windows(2) {
        assert!(
            pair[0].index <= pair[1].index,
            "candidate indices should be stable and source ordered: {proof_boundaries:?}"
        );
    }

    let retry_position = proof_boundaries
        .iter()
        .find_map(|candidate| match candidate.selector {
            ProofBoundarySelector::Default => None,
            ProofBoundarySelector::Index { index } => Some(ProofPositionSelector::Index { index }),
        })
        .expect("fixture should expose at least one index candidate");
    let retry = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration,
            proof_position: retry_position,
            include_boundaries: false,
            include_expected_type: false,
            project: None,
        },
    )
    .await
    .expect("retry proof_state with candidate selector");
    let ProofStateResult::Context {
        unavailable,
        goals_before,
        goals_after,
        ..
    } = retry.result.expect("retry proof-state result")
    else {
        panic!("expected retry context payload");
    };
    assert!(
        unavailable.is_empty(),
        "candidate selector should resolve without another unavailable message: {unavailable:?}"
    );
    assert!(
        !goals_before.is_empty() || !goals_after.is_empty(),
        "candidate selector should return usable proof goals"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn verify_resolves_every_declaration_scan_form_by_name() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let file = PathBuf::from("LeanRsFixture/ScanForms.lean");

    // Every syntax form the candidate scan used to drop verifies by name
    // instead of returning not_found (the kan-proofs field report).
    for name in [
        "LeanRsFixture.ScanForms.multi",
        "LeanRsFixture.ScanForms.zeroOrSucc",
        "LeanRsFixture.ScanForms.origin",
        "LeanRsFixture.ScanForms.Point",
        "LeanRsFixture.ScanForms.Default",
        "LeanRsFixture.ScanForms.namedDefault",
    ] {
        let verified = verify_declaration(
            &ctx,
            VerifyDeclarationRequest {
                file: file.clone(),
                declaration: name.to_owned(),
                project: None,
                allow_sorry: false,
                report_axioms: false,
                retry_tainted_non_positive: false,
            },
        )
        .await
        .unwrap_or_else(|err| panic!("verify {name} should complete: {err}"));
        let result = verified.result.expect("verification result");
        assert!(
            matches!(
                result,
                DeclarationVerificationResult::Ok {
                    ref verification_status,
                    ..
                } if verification_status == "verified"
            ),
            "{name} should verify by name, got {result:?}"
        );
    }

    // The anonymous instance resolves under its generated name: discover it
    // through the declaration inventory, then verify that name.
    let inventory = lean_lookup(
        &ctx,
        semantic_request(
            "declarations",
            serde_json::json!({
                "target": { "kind": "file", "path": "LeanRsFixture/ScanForms.lean" },
                "limit": 20
            }),
        ),
    )
    .await
    .expect("scan-forms declaration inventory");
    let inventory_data = semantic_data(inventory);
    let generated = inventory_data["declarations"]
        .as_array()
        .expect("inventory rows")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .find(|name| name.contains("instDefaultPoint"))
        .unwrap_or_else(|| panic!("anonymous instance should be catalogued under its generated name: {inventory_data}"))
        .to_owned();
    let verified = verify_declaration(
        &ctx,
        VerifyDeclarationRequest {
            file,
            declaration: generated.clone(),
            project: None,
            allow_sorry: false,
            report_axioms: false,
            retry_tainted_non_positive: false,
        },
    )
    .await
    .expect("verify anonymous instance");
    let result = verified.result.expect("verification result");
    assert!(
        matches!(
            result,
            DeclarationVerificationResult::Ok {
                ref verification_status,
                ..
            } if verification_status == "verified"
        ),
        "anonymous instance should verify under {generated}, got {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn context_trim_defaults_omit_boundaries_expected_type_and_echo_fields() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let declaration = "LeanRsFixture.ProofActions.stepTheorem".to_owned();

    // Default request: the declaration HAS tactics (so boundary data exists
    // on the wire) and a goal with an expected type, yet the response must
    // carry none of the opt-in or echo keys — the projection never
    // materializes them, proving the skipped code path, not just empty JSON.
    let lean = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: declaration.clone(),
            proof_position: ProofPositionSelector::default(),
            include_boundaries: false,
            include_expected_type: false,
            project: None,
        },
    )
    .await
    .expect("default proof_state");
    let lean_json = serde_json::to_value(lean.result.as_ref().expect("default proof result")).unwrap();
    for key in [
        "proof_boundaries",
        "expected_type",
        "declaration_name",
        "namespace_name",
    ] {
        assert!(
            lean_json.get(key).is_none(),
            "default request must not carry {key}: {lean_json}"
        );
    }
    assert!(
        lean_json["goals_before"]
            .as_array()
            .is_some_and(|goals| !goals.is_empty()),
        "default response still carries the goals: {lean_json}"
    );

    // Opt-in request: same declaration, both flags set — boundaries and the
    // expected type return exactly as before the trim.
    let full = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration,
            proof_position: ProofPositionSelector::default(),
            include_boundaries: true,
            include_expected_type: true,
            project: None,
        },
    )
    .await
    .expect("opt-in proof_state");
    let full_json = serde_json::to_value(full.result.as_ref().expect("opt-in proof result")).unwrap();
    assert!(
        full_json["proof_boundaries"].as_array().is_some_and(|b| !b.is_empty()),
        "include_boundaries: true returns the boundary list for a tactic block: {full_json}"
    );
    // `expected_type` is absent even under the opt-in at a tactic position:
    // the shim populates it only in the cursor-based `runProofState` term arm
    // (InfoTree.lean `runProofState`), while `ProofStateInDeclaration` always
    // reports `none`. The flag is honest forward plumbing, verified here to
    // not invent data.
    assert!(
        full_json.get("expected_type").is_none(),
        "tactic positions carry no expected_type on the wire: {full_json}"
    );
    // The echo fields stay removed even under the opt-ins.
    assert!(full_json.get("declaration_name").is_none());
    assert!(full_json.get("namespace_name").is_none());

    // Quantify the trim on this representative call for the result note.
    eprintln!(
        "context-trim sizes: default={}B opt_in={}B",
        serde_json::to_string(&lean_json).unwrap().len(),
        serde_json::to_string(&full_json).unwrap().len()
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn try_proof_step_batch_returns_all_ordered_rows_under_worker_limit() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let snippets = (0..10).map(|_| "trivial".to_owned()).collect::<Vec<_>>();

    let response = try_proof_step(
        &ctx,
        TryProofStepRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::Default,
            project: None,
            snippet: None,
            snippets,
            retry_tainted_non_positive: false,
        },
    )
    .await
    .expect("batch proof_step");
    let ProofAttemptResult::Ok { result, .. } = response.result.expect("batch proof-step result") else {
        panic!("expected ok proof-step result");
    };
    assert_eq!(result.candidate_limit, 16);
    assert!(!result.candidates_truncated);
    assert_eq!(result.candidates.len(), 10);
    assert_eq!(result.summary.requested_candidates, 10);
    assert_eq!(result.summary.returned_candidates, 10);
    assert_eq!(result.summary.closed, 10);
    assert_eq!(result.summary.budget_exceeded, 0);
    assert_eq!(result.summary.not_attempted, 0);
    assert!(
        result
            .candidates
            .iter()
            .enumerate()
            .all(|(idx, candidate)| candidate.id == format!("candidate_{}", idx + 1)),
        "candidate rows should preserve request order: {:?}",
        result.candidates
    );
    assert!(
        !result.entry_goals.is_empty(),
        "trial envelope must carry the entry goals once per batch: {:?}",
        result.entry_goals
    );
    assert!(
        result.entry_goals.iter().any(|goal| goal.value.contains("True")),
        "entry goals at the default position of stepTheorem are the pristine True goal: {:?}",
        result.entry_goals
    );
    assert!(
        result.locals.is_empty(),
        "stepTheorem has no hypotheses at its pristine goal: {:?}",
        result.locals
    );
    for candidate in &result.candidates {
        assert_eq!(candidate.status, "closed");
        assert!(
            candidate
                .diagnostics
                .diagnostics
                .iter()
                .chain(candidate.downstream_diagnostics.diagnostics.iter())
                .all(|diagnostic| !matches!(diagnostic.severity, Severity::Error)),
            "a closed candidate's diagnostics carry no error-severity entries: {:?}",
            candidate.diagnostics
        );
    }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn trial_loop_runs_with_zero_per_step_context_calls() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let declaration = "LeanRsFixture.ProofActions.stepTheorem".to_owned();

    // The intended loop: ONE navigation call per declaration...
    let navigate = proof_state(
        &ctx,
        ProofStateRequest {
            file: proof_actions_file(),
            declaration: declaration.clone(),
            proof_position: ProofPositionSelector::default(),
            include_boundaries: true,
            include_expected_type: false,
            project: None,
        },
    )
    .await
    .expect("navigation proof_state");
    let navigate_json = serde_json::to_value(navigate.result.as_ref().expect("navigation result")).unwrap();

    // ...then every step is a self-contained trial. Two batches at two
    // positions stand in for a stepping loop; no lean_context call between
    // them, and each envelope re-proves it carries its own entry state.
    let mut envelope_bytes = Vec::new();
    for position in [
        ProofPositionSelector::Default,
        ProofPositionSelector::Index { index: 0 },
    ] {
        let response = try_proof_step(
            &ctx,
            TryProofStepRequest {
                file: proof_actions_file(),
                declaration: declaration.clone(),
                proof_position: position,
                project: None,
                snippet: None,
                snippets: vec!["trivial".to_owned(), "skip".to_owned()],
                retry_tainted_non_positive: false,
            },
        )
        .await
        .expect("trial batch");
        let ProofAttemptResult::Ok { result, .. } = response.result.expect("trial batch result") else {
            panic!("expected ok proof-step result");
        };
        assert!(
            !result.entry_goals.is_empty(),
            "every envelope in the loop carries its entry goals: {:?}",
            result.entry_goals
        );
        envelope_bytes.push(serde_json::to_string(&result).unwrap().len());
    }
    // The eliminated per-step cost is a full context response per trial.
    eprintln!(
        "trial-loop sizes: navigation={}B envelopes={envelope_bytes:?}B (per-step lean_context eliminated)",
        serde_json::to_string(&navigate_json).unwrap().len()
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn try_proof_step_partial_budget_surfaces_batch_summary() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx_with_config(
        &root,
        ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            output: OutputBudgetOverrides {
                max_total_bytes: Some(1024),
                ..OutputBudgetOverrides::default()
            },
            ..ToolConfig::default()
        },
    );
    let snippets = (0..16)
        .map(|idx| format!("exact definitely_missing_identifier_with_a_long_name_to_fill_budget_{idx}"))
        .collect::<Vec<_>>();

    let response = try_proof_step(
        &ctx,
        TryProofStepRequest {
            file: proof_actions_file(),
            declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
            proof_position: ProofPositionSelector::Default,
            project: None,
            snippet: None,
            snippets,
            retry_tainted_non_positive: false,
        },
    )
    .await
    .expect("partial proof_step");
    let warnings = response.warnings.clone();
    let ProofAttemptResult::Ok { result, .. } = response.result.expect("partial proof-step result") else {
        panic!("expected ok proof-step result");
    };
    assert!(result.summary.partial, "expected a partial batch summary: {result:?}");
    assert!(
        result.summary.budget_exceeded > 0 || result.summary.not_attempted > 0 || result.summary.output_truncated > 0,
        "partial summary should identify the limiting budget fact: {:?}",
        result.summary
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("partial output")),
        "partial proof-step response should warn clearly: {warnings:?}"
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
            include_boundaries: false,
            include_expected_type: false,
            project: None,
        },
    )
    .await
    .expect("proof_state");
    let ProofStateResult::Context {
        goals_after,
        query_facts,
        ..
    } = proof.result.expect("proof result")
    else {
        panic!("expected proof context");
    };
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
            include_boundaries: false,
            include_expected_type: false,
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
            retry_tainted_non_positive: false,
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
    let diagnostic = result.candidates[0]
        .diagnostics
        .diagnostics
        .iter()
        .find(|d| d.message.contains("definitely_missing_identifier"))
        .expect("missing identifier diagnostic present");
    assert_eq!(diagnostic.coordinate_space, CoordinateSpace::SyntheticBuffer);
    assert!(
        diagnostic.synthetic_range.is_some(),
        "proof-step diagnostics should expose the synthetic trial range"
    );
    assert!(
        diagnostic.original_range.is_none(),
        "synthetic proof-step diagnostics should not pretend to have original source ranges"
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
            retry_tainted_non_positive: false,
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
            retry_tainted_non_positive: false,
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
            semantic_request_without_kind(serde_json::json!({
                "targets": [{
                    "kind": "explicit",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declarations": ["LeanRsFixture.ProofActions.closedTheorem"]
                }],
                "report_axioms": true
            })),
        )
        .await
        .expect("lean_verify targets"),
    );
    assert_eq!(
        verified
            .pointer("/results/0/verification_status")
            .and_then(serde_json::Value::as_str),
        Some("verified")
    );

    let refs_response = lean_lookup(
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
    .expect("lean_lookup references");
    assert_ne!(
        refs_response.trust.session_id, "metadata-only",
        "file-scope references elaborate the source snapshot and should carry a worker session"
    );
    assert!(
        refs_response.trust.artifacts.iter().any(|artifact| {
            artifact.artifact == lean_host_mcp::ArtifactKind::Source
                && artifact.scope == lean_host_mcp::TrustScope::File
                && artifact.status == lean_host_mcp::TrustStatus::EditFresh
                && artifact.path.as_deref() == Some("LeanRsFixture/ProofActions.lean")
        }),
        "file-scope references should report source edit-fresh trust: {:?}",
        refs_response.trust.artifacts
    );
    let refs = semantic_data(refs_response);
    assert_eq!(refs.pointer("/status").and_then(serde_json::Value::as_str), Some("ok"));
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn lean_verify_batches_explicit_file_and_module_targets() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let explicit = semantic_data(
        lean_verify(
            &ctx,
            semantic_request_without_kind(serde_json::json!({
                "targets": [{
                    "kind": "explicit",
                    "file": "LeanRsFixture/ProofActions.lean",
                    "declarations": [
                        "LeanRsFixture.ProofActions.closedTheorem",
                        "LeanRsFixture.ProofActions.sorryTheorem",
                        "LeanRsFixture.ProofActions.notFound"
                    ]
                }],
                "allow_sorry": false
            })),
        )
        .await
        .expect("lean_verify explicit batch"),
    );
    assert_eq!(explicit.pointer("/summary/requested"), Some(&serde_json::json!(3)));
    assert_eq!(explicit.pointer("/summary/verified"), Some(&serde_json::json!(1)));
    assert_eq!(explicit.pointer("/summary/failed"), Some(&serde_json::json!(2)));
    let statuses = explicit["results"]
        .as_array()
        .expect("explicit results")
        .iter()
        .map(|row| row["verification_status"].as_str().expect("status").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(statuses, ["verified", "has_sorry", "not_found"]);

    let file_all = semantic_data(
        lean_verify(
            &ctx,
            semantic_request_without_kind(serde_json::json!({
                "targets": [{ "kind": "file_all", "file": "LeanRsFixture/ProofActions.lean" }],
                "allow_sorry": true
            })),
        )
        .await
        .expect("lean_verify file_all"),
    );
    assert!(
        file_all
            .pointer("/summary/requested")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|requested| requested >= 4),
        "file_all should verify every fixture declaration: {file_all:?}"
    );
    assert_eq!(file_all.pointer("/summary/needs_build"), Some(&serde_json::json!(0)));

    let module_all = lean_verify(
        &ctx,
        semantic_request_without_kind(serde_json::json!({
            "targets": [{ "kind": "module_all", "module": "LeanRsFixture.ProofActions" }],
            "allow_sorry": true
        })),
    )
    .await
    .expect("lean_verify module_all");
    assert!(
        module_all.trust.artifacts.iter().any(|artifact| {
            artifact.artifact == lean_host_mcp::ArtifactKind::Source
                && artifact.status == lean_host_mcp::TrustStatus::EditFresh
        }),
        "source-backed module_all should report source edit-fresh trust: {:?}",
        module_all.trust.artifacts
    );
    let module_all = semantic_data(module_all);
    assert_eq!(
        file_all.pointer("/summary/requested"),
        module_all.pointer("/summary/requested"),
        "module_all source path should enumerate the same declarations as file_all"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn lean_verify_normalizes_paths_dedupes_trust_and_compacts_rows() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);
    let response = lean_verify(
        &ctx,
        semantic_request_without_kind(serde_json::json!({
            "targets": [
                { "kind": "explicit", "file": "LeanRsFixture/ProofActions.lean", "declarations": ["LeanRsFixture.ProofActions.closedTheorem"] },
                { "kind": "file_all", "file": "LeanRsFixture/ProofActions.lean" },
                { "kind": "module_all", "module": "LeanRsFixture.ProofActions" }
            ],
            "allow_sorry": true
        })),
    )
    .await
    .expect("lean_verify mixed targets");

    let mut artifact_values = std::collections::BTreeSet::new();
    for artifact in &response.trust.artifacts {
        let encoded = serde_json::to_string(artifact).expect("trust artifact encodes");
        assert!(
            artifact_values.insert(encoded),
            "duplicate trust artifact: {artifact:?}"
        );
    }

    let data = semantic_data(response);
    let rows = data["results"].as_array().expect("verify results");
    assert!(!rows.is_empty(), "verify should produce rows: {data:?}");
    for row in rows {
        assert_eq!(
            row["file"].as_str(),
            Some("LeanRsFixture/ProofActions.lean"),
            "every row should use project-relative file paths: {row:?}"
        );
        assert!(
            row.pointer("/facts/target").is_none(),
            "compact verify rows should not repeat target span blocks: {row:?}"
        );
        assert!(
            row.pointer("/facts/candidates")
                .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
            "compact verify rows should not repeat ambiguous candidate span blocks: {row:?}"
        );
    }

    let full = semantic_data(
        lean_verify(
            &ctx,
            semantic_request_without_kind(serde_json::json!({
                "targets": [{ "kind": "explicit", "file": "LeanRsFixture/ProofActions.lean", "declarations": ["LeanRsFixture.ProofActions.closedTheorem"] }],
                "allow_sorry": true,
                "detail": "full"
            })),
        )
        .await
        .expect("lean_verify full detail"),
    );
    assert!(
        full.pointer("/results/0/facts/target").is_some(),
        "full detail should preserve target span facts: {full:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn lean_trial_command_and_lean_status_file_diagnostics() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let command = semantic_data(
        lean_trial(
            &ctx,
            semantic_request(
                "command",
                serde_json::json!({
                    "imports": ["Init"],
                    "commands": "#check Nat.add\n#print axioms Nat.add_assoc"
                }),
            ),
        )
        .await
        .expect("lean_trial command explicit imports"),
    );
    assert!(
        command
            .pointer("/output/value")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|output| output.contains("Nat.add") && output.contains("Nat.add_assoc")),
        "command output should contain #check and #print axioms messages: {command:?}"
    );
    assert_eq!(
        command
            .pointer("/diagnostics/summary/errors")
            .and_then(serde_json::Value::as_u64),
        Some(0)
    );

    let file_command = semantic_data(
        lean_trial(
            &ctx,
            semantic_request(
                "command",
                serde_json::json!({
                    "file": "LeanRsFixture/ProofActions.lean",
                    "commands": "#check LeanRsFixture.ProofActions.closedTheorem"
                }),
            ),
        )
        .await
        .expect("lean_trial command file imports"),
    );
    assert!(
        file_command
            .pointer("/output/value")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|output| output.contains("closedTheorem")),
        "file-derived command should see declarations from the current source: {file_command:?}"
    );

    let invalid = semantic_data(
        lean_trial(
            &ctx,
            semantic_request(
                "command",
                serde_json::json!({
                    "imports": ["Init"],
                    "commands": "#check definitelyMissingCommandMessage"
                }),
            ),
        )
        .await
        .expect("lean_trial invalid command"),
    );
    assert!(
        invalid
            .pointer("/diagnostics/summary/errors")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|errors| errors > 0),
        "invalid command should return diagnostics, not fail transport: {invalid:?}"
    );

    let tmp = tempfile::tempdir().expect("temp fixture copy");
    let work = tmp.path().join("fixture");
    copy_dir_recursive(&root, &work);
    let proof_actions = work.join("LeanRsFixture/ProofActions.lean");
    let edited =
        fs::read_to_string(&proof_actions).expect("read proof actions") + "\n#check definitelyMissingFileDiagnostic\n";
    fs::write(&proof_actions, edited).expect("write diagnostics edit");
    let ctx = open_ctx(&work);
    let diagnostics = lean_status(
        &ctx,
        semantic_request(
            "file_diagnostics",
            serde_json::json!({
                "file": "LeanRsFixture/ProofActions.lean"
            }),
        ),
    )
    .await
    .expect("lean_status file diagnostics");
    assert!(
        diagnostics.trust.artifacts.iter().any(|artifact| {
            artifact.artifact == lean_host_mcp::ArtifactKind::Source
                && artifact.status == lean_host_mcp::TrustStatus::EditFresh
        }),
        "file diagnostics should report source edit-fresh trust: {:?}",
        diagnostics.trust.artifacts
    );
    let diagnostics = semantic_data(diagnostics);
    assert!(
        diagnostics
            .pointer("/diagnostics/summary/errors")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|errors| errors > 0),
        "file diagnostics should report current-source errors: {diagnostics:?}"
    );
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
async fn lean_lookup_declarations_file_and_module_are_source_fresh() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let ctx = open_ctx(&root);

    let file = lean_lookup(
        &ctx,
        semantic_request(
            "declarations",
            serde_json::json!({
                "target": { "kind": "file", "path": "LeanRsFixture/ProofAgent.lean" },
                "limit": 20
            }),
        ),
    )
    .await
    .expect("file declaration inventory");
    assert!(
        file.errors
            .iter()
            .all(|issue| issue.severity.as_deref() != Some("error")),
        "file inventory should not carry error-severity issues: {:?}",
        file.errors
    );
    assert!(file.trust.artifacts.iter().any(|artifact| {
        artifact.artifact == lean_host_mcp::ArtifactKind::Source
            && artifact.status == lean_host_mcp::TrustStatus::EditFresh
    }));
    let file_data = semantic_data(file);
    assert_eq!(file_data["status"], "ok");
    assert_eq!(file_data["source"], "worker");
    let file_names = file_data["declarations"]
        .as_array()
        .expect("declarations array")
        .iter()
        .map(|row| row["name"].as_str().expect("name").to_owned())
        .collect::<Vec<_>>();
    assert!(
        file_names.contains(&"LeanRsFixture.ProofAgent.miniRatDenominatorStep".to_owned()),
        "file declaration inventory should include theorem, got {file_names:?}"
    );

    let module = lean_lookup(
        &ctx,
        semantic_request(
            "declarations",
            serde_json::json!({
                "target": { "kind": "module", "module": "LeanRsFixture.ProofAgent" },
                "limit": 20
            }),
        ),
    )
    .await
    .expect("module declaration inventory");
    assert!(
        module
            .errors
            .iter()
            .all(|issue| issue.severity.as_deref() != Some("error")),
        "module inventory should not carry error-severity issues: {:?}",
        module.errors
    );
    assert!(module.trust.artifacts.iter().any(|artifact| {
        artifact.artifact == lean_host_mcp::ArtifactKind::Source
            && artifact.status == lean_host_mcp::TrustStatus::EditFresh
    }));
    let module_data = semantic_data(module);
    assert_eq!(module_data["status"], "ok");
    assert_eq!(module_data["source"], "worker");
    let module_names = module_data["declarations"]
        .as_array()
        .expect("declarations array")
        .iter()
        .map(|row| row["name"].as_str().expect("name").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(file_names, module_names);
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
            include_boundaries: false,
            include_expected_type: false,
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
            retry_tainted_non_positive: false,
        },
    );

    let (proof, inspect, verify) = tokio::join!(proof, inspect, verify);
    let proof = proof.expect("proof_state should complete");
    let inspect = inspect.expect("inspect_declaration should complete");
    let verify = verify.expect("verify_declaration should complete");

    assert!(proof.runtime().is_some(), "proof_state should include runtime facts");
    assert!(
        verify.runtime().is_some(),
        "verify_declaration should include runtime facts"
    );

    // One project means one actor thread running one job at a time, so
    // concurrent calls against it are serialized by its mailbox. At least one
    // of the three must therefore observe a nonzero mailbox wait. This asserts
    // the *shape* of same-project concurrency, not a latency budget — see
    // `multi_project::concurrent_calls_across_projects_do_not_queue` for the
    // complementary claim that *different* projects do not serialize.
    let waits: Vec<u64> = [proof.runtime(), inspect.runtime(), verify.runtime()]
        .iter()
        .map(|runtime| runtime.map_or(0, |facts| facts.queue_wait_millis))
        .collect();
    assert!(
        waits.iter().any(|wait| *wait > 0),
        "concurrent same-project calls must queue behind the project's single actor; queue_wait_millis={waits:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
#[serial(handles_olean)]
async fn rebuilding_an_imported_olean_recycles_the_worker_before_the_next_call() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = open_ctx(&root);
    let imports = vec!["LeanRsFixture.Handles".to_owned()];
    let request = || InspectDeclarationRequest {
        name: "Nat.add_zero".to_owned(),
        file: None,
        imports: imports.clone(),
        project: None,
        raw_statement: false,
        fields: InspectDeclarationFields::default(),
    };

    // First call opens the session and stamps the import's artifact.
    let first = inspect_declaration(&ctx, request()).await.expect("first inspect");
    let first_generation = first
        .runtime()
        .expect("full verbosity keeps runtime facts")
        .worker_generation;

    // A second identical call must reuse everything: same session, no recycle.
    // Asserted first so the recycle below is attributable to the mtime bump
    // and not to something this workload does on every repeat.
    let steady = inspect_declaration(&ctx, request()).await.expect("second inspect");
    let steady_runtime = steady.runtime().expect("full verbosity keeps runtime facts");
    assert!(
        steady_runtime.call_restart.is_none(),
        "a repeat call must not recycle: {:?}",
        steady_runtime.call_restart
    );
    assert_eq!(steady_runtime.worker_generation, first_generation);

    // Now stand in for `lake build`: move the imported module's `.olean`
    // mtime forward. Forward, not backward, so the artifact stays newer than
    // its source and Lake still considers the fixture built.
    let olean = root.join(".lake/build/lib/lean/LeanRsFixture/Handles.olean");
    let bumped = std::fs::metadata(&olean)
        .expect("fixture must be built")
        .modified()
        .unwrap()
        + std::time::Duration::from_mins(1);
    std::fs::File::options()
        .write(true)
        .open(&olean)
        .expect("open the imported olean")
        .set_modified(bumped)
        .expect("bump the olean mtime");

    let after = inspect_declaration(&ctx, request()).await.expect("third inspect");
    let after_runtime = after.runtime().expect("full verbosity keeps runtime facts");
    assert_eq!(
        after_runtime.call_restart.as_ref().map(|event| event.cause.as_str()),
        Some("artifacts_rebuilt"),
        "a rebuilt import must recycle the worker so the next session re-imports; runtime={after_runtime:?}"
    );
    assert!(
        after_runtime.worker_generation > first_generation,
        "the recycle must actually advance the worker generation: {} -> {}",
        first_generation,
        after_runtime.worker_generation
    );
    // The recycle is pre-job, so the answer is still a real one.
    assert!(matches!(after.status, lean_host_mcp::ResponseStatus::Ok));
}

/// The proactive half of the residue policy: when a project falls quiet above
/// the soft budget, the actor cycles the child *between* calls rather than
/// inside the one that would have crossed the hard budget.
///
/// Forced with a one-byte budget so the fixture, whose imports are orders of
/// magnitude smaller than a Mathlib-scale profile, still crosses it — the
/// quantity under test is the actor's timing, not the child's arithmetic, which
/// `lean-rs-worker-parent`'s own suite pins against known byte counts.
#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
#[serial(handles_olean)]
async fn an_idle_project_over_its_soft_budget_cycles_between_calls() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = open_ctx_with_residue_budget(&root, 1);
    let request = || InspectDeclarationRequest {
        name: "Nat.add_zero".to_owned(),
        file: None,
        imports: vec!["LeanRsFixture.Handles".to_owned()],
        project: None,
        raw_statement: false,
        fields: InspectDeclarationFields::default(),
    };

    let first = inspect_declaration(&ctx, request()).await.expect("first inspect");
    assert!(matches!(first.status, lean_host_mcp::ResponseStatus::Ok));

    // Longer than the actor's quiescence grace, so the receive times out and the
    // idle path runs. Nothing is in flight, so this is wall time, not a race.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let second = inspect_declaration(&ctx, request()).await.expect("second inspect");
    let runtime = second.runtime().expect("full verbosity keeps runtime facts");
    assert!(
        runtime.restarts_by_cause.contains_key("import_residue_idle"),
        "a quiet project over its soft budget must cycle proactively; causes={:?}",
        runtime.restarts_by_cause
    );
    assert!(
        runtime.call_restart.is_none(),
        "an idle cycle happens between calls, so it must never be attributed to one: {:?}",
        runtime.call_restart
    );
    // The pre-warm re-imported the profile this call asks for, so the call is
    // answered by a child that is both fresh and already warm.
    assert!(matches!(second.status, lean_host_mcp::ResponseStatus::Ok));
}

/// The control for the test above: under the shipped budget the fixture never
/// approaches the soft threshold, so no residue cycle of either kind fires and
/// the idle path costs nothing — not a wakeup, not a respawn.
#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
#[serial(handles_olean)]
async fn a_project_under_its_soft_budget_never_cycles_for_residue() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = open_ctx(&root);
    let request = || InspectDeclarationRequest {
        name: "Nat.add_zero".to_owned(),
        file: None,
        imports: vec!["LeanRsFixture.Handles".to_owned()],
        project: None,
        raw_statement: false,
        fields: InspectDeclarationFields::default(),
    };

    let first = inspect_declaration(&ctx, request()).await.expect("first inspect");
    let first_generation = first
        .runtime()
        .expect("full verbosity keeps runtime facts")
        .worker_generation;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let second = inspect_declaration(&ctx, request()).await.expect("second inspect");
    let runtime = second.runtime().expect("full verbosity keeps runtime facts");
    assert_eq!(
        runtime.worker_generation, first_generation,
        "an idle project under its budget must not be cycled at all"
    );
    for cause in ["import_residue", "import_residue_idle"] {
        assert!(
            !runtime.restarts_by_cause.contains_key(cause),
            "{cause} must not fire under the shipped budget; causes={:?}",
            runtime.restarts_by_cause
        );
    }
}

/// A rebuild must be caught even when another import profile ran in between.
///
/// The child pools sessions, so the profile served two calls ago is still alive
/// and would be restored without re-importing — from `.olean` files that have
/// since changed. Before the pool a differing profile always re-imported, so
/// remembering only the immediately preceding profile was enough; this pins the
/// property that survived that change.
#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
#[serial(handles_olean)]
async fn a_rebuild_recycles_even_when_another_profile_ran_in_between() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = open_ctx(&root);
    let request = |module: &str| InspectDeclarationRequest {
        name: "Nat.add_zero".to_owned(),
        file: None,
        imports: vec![module.to_owned()],
        project: None,
        raw_statement: false,
        fields: InspectDeclarationFields::default(),
    };
    // Profile A stamps its artifact, then profile B takes over as the live one.
    let first = inspect_declaration(&ctx, request("LeanRsFixture.Handles"))
        .await
        .expect("inspect under profile A");
    let first_generation = first
        .runtime()
        .expect("full verbosity keeps runtime facts")
        .worker_generation;
    let other = inspect_declaration(&ctx, request("LeanRsFixture.Strings"))
        .await
        .expect("inspect under profile B");
    let other_runtime = other.runtime().expect("full verbosity keeps runtime facts");
    assert!(
        other_runtime.call_restart.is_none(),
        "switching profiles must not recycle on its own: {:?}",
        other_runtime.call_restart
    );

    // Stand in for `lake build` on A's import only. Forward, so the artifact
    // stays newer than its source and Lake still considers the fixture built.
    let olean = root.join(".lake/build/lib/lean/LeanRsFixture/Handles.olean");
    let bumped = std::fs::metadata(&olean)
        .expect("fixture must be built")
        .modified()
        .unwrap()
        + std::time::Duration::from_mins(1);
    std::fs::File::options()
        .write(true)
        .open(&olean)
        .expect("open the imported olean")
        .set_modified(bumped)
        .expect("bump the olean mtime");

    let back = inspect_declaration(&ctx, request("LeanRsFixture.Handles"))
        .await
        .expect("inspect back under profile A");
    let back_runtime = back.runtime().expect("full verbosity keeps runtime facts");
    assert_eq!(
        back_runtime.call_restart.as_ref().map(|event| event.cause.as_str()),
        Some("artifacts_rebuilt"),
        "returning to a rebuilt profile must recycle rather than reuse the pooled session; runtime={back_runtime:?}"
    );
    assert!(
        back_runtime.worker_generation > first_generation,
        "the recycle must actually advance the worker generation: {} -> {}",
        first_generation,
        back_runtime.worker_generation
    );
    assert!(matches!(back.status, lean_host_mcp::ResponseStatus::Ok));

    // And it must not recycle again: the fresh child imported A at the new
    // stamp, so the very next call has nothing left to invalidate.
    let settled = inspect_declaration(&ctx, request("LeanRsFixture.Handles"))
        .await
        .expect("inspect after the recycle");
    let settled_runtime = settled.runtime().expect("full verbosity keeps runtime facts");
    assert!(
        settled_runtime.call_restart.is_none(),
        "one rebuild must cost exactly one recycle: {:?}",
        settled_runtime.call_restart
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn batched_declaration_searches_answer_in_request_order() {
    let Some(root) = fixture_root() else {
        return;
    };
    let ctx = open_ctx(&root);

    // Three fragments, no one of which is a substring of another, each matching
    // a distinct fixture declaration. `search_for_proof` re-pairs the batch's
    // results to their labels *positionally*, so order is the contract: with
    // these inputs any permutation — a swap or a rotation — fails, which no
    // weaker assertion (counts, non-emptiness, a set of names) can detect.
    let fragments = ["stringIdentity", "levelSucc", "exprBVar"];
    let requests: Vec<_> = fragments
        .iter()
        .map(|fragment| LeanWorkerDeclarationSearch {
            name_fragment: Some((*fragment).to_owned()),
            name_match: LeanWorkerDeclarationNameMatch::Contains,
            kind: None,
            required_constants: Vec::new(),
            conclusion_head: None,
            scope_biases: Vec::new(),
            limit: 8,
            filter: LeanWorkerDeclarationFilter {
                include_private: false,
                include_generated: false,
                include_internal: false,
            },
            include_source: false,
        })
        .collect();

    let imports = vec![
        "Init".to_owned(),
        "LeanRsFixture.Strings".to_owned(),
        "LeanRsFixture.Handles".to_owned(),
    ];
    let call = ctx
        .broker
        .search_declarations(ProjectHint::from_request(None), imports.clone(), imports, requests)
        .await
        .expect("batched declaration search should complete");

    assert_eq!(
        call.value.len(),
        fragments.len(),
        "the batch must answer every request exactly once"
    );
    for (fragment, result) in fragments.iter().zip(&call.value) {
        let names: Vec<&str> = result.declarations.iter().map(|row| row.name.as_str()).collect();
        assert!(
            !names.is_empty(),
            "fragment {fragment} must match at least one fixture declaration, or the ordering \
             assertion below is vacuous"
        );
        // Case-insensitively: the worker's `Contains` match is, so `levelSucc`
        // legitimately answers with `Lean.mkLevelSucc`. Folding case keeps the
        // three fragments pairwise distinguishable, which is all the oracle needs.
        let needle = fragment.to_lowercase();
        assert!(
            names.iter().all(|name| name.to_lowercase().contains(&needle)),
            "result for {fragment} answers a different request: {names:?}"
        );
    }
}
