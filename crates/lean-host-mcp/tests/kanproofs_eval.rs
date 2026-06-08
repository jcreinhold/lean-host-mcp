//! Opt-in `KanProofs` field evaluation for the semantic proof-search lane.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::significant_drop_tightening,
    clippy::unwrap_used,
    reason = "manual ignored field-eval harness prints compact JSON summaries and keeps one ToolContext per run"
)]

use std::path::{Path, PathBuf};
use std::time::Instant;

use lean_host_mcp::tools::position::ProofPositionSelector;
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
use lean_host_mcp::tools::{TelemetryVerbosity, ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker};
use serde::Serialize;

fn kanproofs_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_KANPROOFS_EVAL").ok().map(PathBuf::from)
}

fn kanproofs_unbuilt_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_KANPROOFS_UNBUILT_EVAL")
        .ok()
        .map(PathBuf::from)
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
        config: ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    }
}

#[derive(Serialize)]
struct EvalCandidate {
    name: String,
    module: Option<String>,
    score: i32,
    match_reason: String,
}

#[derive(Serialize)]
struct EvalSummary {
    label: &'static str,
    elapsed_ms: u128,
    imports: Vec<String>,
    warnings: Vec<String>,
    result_warnings: Vec<String>,
    telemetry_imports: Vec<String>,
    runtime: serde_json::Value,
    candidates: Vec<EvalCandidate>,
}

struct Query {
    label: &'static str,
    file: &'static str,
    declaration: &'static str,
}

#[tokio::test]
#[ignore = "manual KanProofs semantic field eval; set LEAN_HOST_MCP_KANPROOFS_EVAL"]
async fn kanproofs_semantic_search_field_eval() {
    let Some(root) = kanproofs_root() else {
        panic!("LEAN_HOST_MCP_KANPROOFS_EVAL not set");
    };
    let ctx = open_ctx(&root);
    let queries = [
        Query {
            label: "small_rat_denominator",
            file: "KanProofs/Data/Rat/Lemmas.lean",
            declaration: "Rat.exists_intCast_eq_intCast_mul_of_den_dvd",
        },
        Query {
            label: "broad_kummer_roots_of_unity",
            file: "KanProofs/Algebra/Category/ContinuousCohomology/Kummer/RootsOfUnity.lean",
            declaration: "ContinuousCohomology.rootsOfUnity_smul_coe",
        },
        Query {
            label: "int_factorization",
            file: "KanProofs/Data/Int/Factorization.lean",
            declaration: "Int.eq_sign_mul_prod_factorization_natAbs_pow",
        },
    ];

    for query in queries {
        let started = Instant::now();
        let response = search_for_proof(
            &ctx,
            SearchForProofRequest {
                file: Some(PathBuf::from(query.file)),
                declaration: Some(query.declaration.to_owned()),
                proof_position: ProofPositionSelector::default(),
                goal: None,
                type_text: None,
                imports: Vec::new(),
                mode: Some(ProofSearchMode::NextStep),
                limit: Some(8),
                project: None,
            },
        )
        .await
        .expect("search_for_proof");
        let elapsed_ms = started.elapsed().as_millis();
        let result = response.result.expect("search result");
        let telemetry = response.telemetry.expect("full telemetry");
        assert!(
            telemetry
                .imports
                .iter()
                .all(|import| import != "LeanSemanticSearch.Capability"),
            "semantic capability import leaked into telemetry: {:?}",
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
        let telemetry_imports = telemetry.imports;
        let summary = EvalSummary {
            label: query.label,
            elapsed_ms,
            imports: telemetry_imports.clone(),
            warnings: response.warnings,
            result_warnings: result.warnings,
            telemetry_imports,
            runtime: serde_json::to_value(telemetry.runtime).expect("runtime facts encode"),
            candidates: result
                .candidates
                .into_iter()
                .take(5)
                .map(|candidate| EvalCandidate {
                    name: candidate.name,
                    module: candidate.module,
                    score: candidate.score,
                    match_reason: candidate.match_reason,
                })
                .collect(),
        };
        println!("{}", serde_json::to_string(&summary).expect("summary encodes"));
    }
}

#[tokio::test]
#[ignore = "manual KanProofs unbuilt fallback eval; set LEAN_HOST_MCP_KANPROOFS_UNBUILT_EVAL"]
async fn kanproofs_unbuilt_import_degrades_to_build_warning() {
    let Some(root) = kanproofs_unbuilt_root() else {
        panic!("LEAN_HOST_MCP_KANPROOFS_UNBUILT_EVAL not set");
    };
    let ctx = open_ctx(&root);
    let started = Instant::now();
    let response = search_for_proof(
        &ctx,
        SearchForProofRequest {
            file: None,
            declaration: None,
            proof_position: ProofPositionSelector::default(),
            goal: Some("∀ (n : Nat), n = n".to_owned()),
            type_text: None,
            imports: vec!["KanProofs.Algebra.Category.ContinuousCohomology.LowDegree".to_owned()],
            mode: Some(ProofSearchMode::NextStep),
            limit: Some(5),
            project: None,
        },
    )
    .await
    .expect("search_for_proof");
    let elapsed_ms = started.elapsed().as_millis();
    let result = response.result.expect("search result");
    let telemetry = response.telemetry.expect("full telemetry");
    assert!(
        telemetry
            .imports
            .iter()
            .all(|import| import != "LeanSemanticSearch.Capability"),
        "semantic capability import leaked into telemetry: {:?}",
        telemetry.imports
    );
    assert!(
        response
            .warnings
            .iter()
            .chain(result.warnings.iter())
            .any(|warning| warning.contains("lake build") || warning.contains("consumer project imports are not built")),
        "unbuilt import should report lake build guidance, response={:?}, result={:?}",
        response.warnings,
        result.warnings
    );
    let summary = serde_json::json!({
        "label": "unbuilt_explicit_kanproofs_import",
        "elapsed_ms": elapsed_ms,
        "warnings": response.warnings,
        "result_warnings": result.warnings,
        "telemetry_imports": telemetry.imports,
        "runtime": serde_json::to_value(telemetry.runtime).expect("runtime facts encode"),
        "candidate_count": result.candidates.len(),
    });
    println!("{}", serde_json::to_string(&summary).expect("summary encodes"));
}
