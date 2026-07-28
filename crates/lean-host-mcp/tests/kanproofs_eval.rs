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
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectRuntimeConfig};
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

/// What a changed-file sweep actually costs on a Mathlib-scale project, and
/// whether the residue policy behaves as designed while paying it.
///
/// This is the workload the byte-denominated bound exists for:
/// `changed_coverage` loops `declaration_inventory` once per changed file, and
/// on this project 96% of files carry a unique ordered import header — so *N*
/// files means *N* distinct import profiles and *N* imports, each retaining its
/// whole closure. The sweep here calls the same inventory path directly rather
/// than through a git diff, so it needs no working-tree changes in the project
/// under test.
///
/// Three claims, none of which the fixture can exercise because its imports are
/// three orders of magnitude smaller:
///
/// 1. **Every file is answered.** Recycling mid-sweep is invisible to results.
/// 2. **Every recycle is a planned residue cycle.** Nothing crashes, times out,
///    or trips the abnormal-restart breaker under a sweep that deliberately
///    exhausts the budget.
/// 3. **The recycle count matches the budget arithmetic** — at most
///    `imports x residue-per-import / budget`, rounded up, plus one for the
///    generation in flight. More than that means the accounting is wrong; zero
///    where the residue clearly exceeded the budget means it is not firing.
#[tokio::test]
#[ignore = "manual KanProofs residue sweep; multi-GB and minutes long; set LEAN_HOST_MCP_KANPROOFS_EVAL"]
async fn a_changed_file_sweep_recycles_only_on_its_residue_budget() {
    use lean_host_mcp::tools::declaration_inventory::{
        DeclarationInventoryRequest, DeclarationInventoryTarget, declaration_inventory,
    };

    let Some(root) = kanproofs_root() else {
        panic!("LEAN_HOST_MCP_KANPROOFS_EVAL not set");
    };
    // One per unrelated subtree, so the profiles are genuinely distinct rather
    // than near-duplicates of one another.
    let files = [
        "KanProofs/Data/Rat/Lemmas.lean",
        "KanProofs/Data/Int/Factorization.lean",
        "KanProofs/Algebra/Category/ContinuousCohomology/Kummer/RootsOfUnity.lean",
    ];
    let ctx = open_ctx(&root);

    let mut generations = Vec::new();
    let mut causes = std::collections::BTreeMap::new();
    let mut residues = Vec::new();
    for file in files {
        let started = Instant::now();
        let response = declaration_inventory(
            &ctx,
            DeclarationInventoryRequest {
                target: DeclarationInventoryTarget::File {
                    path: PathBuf::from(file),
                },
                project: None,
                limit: None,
            },
        )
        .await
        .expect("declaration_inventory");
        let result = response
            .result_ref()
            .unwrap_or_else(|| panic!("{file} must be answered"));
        assert_eq!(result.status, "ok", "{file} reported {}", result.status);
        let telemetry = response.telemetry.as_ref().expect("full verbosity keeps telemetry");
        let runtime = telemetry.runtime.as_ref().expect("full verbosity keeps runtime facts");
        generations.push(runtime.worker_generation);
        causes.clone_from(&runtime.restarts_by_cause);
        residues.push(runtime.import_residue_bytes.unwrap_or_default());
        println!(
            "{}",
            serde_json::json!({
                "file": file,
                "elapsed_ms": started.elapsed().as_millis(),
                "declarations": result.declarations.len(),
                "worker_generation": runtime.worker_generation,
                "call_restart": runtime.call_restart.as_ref().map(|event| event.cause.clone()),
                "import_residue_mib": runtime.import_residue_bytes.unwrap_or_default() / (1024 * 1024),
                "import_residue_limit_mib": runtime.import_residue_limit_bytes.unwrap_or_default() / (1024 * 1024),
            })
        );
    }

    for cause in causes.keys() {
        assert!(
            matches!(
                cause.as_str(),
                "import_residue" | "import_residue_idle" | "artifacts_rebuilt"
            ),
            "a sweep must only ever take planned cycles; saw {cause} in {causes:?}"
        );
    }
    let cycles = generations.last().copied().unwrap_or_default() - generations.first().copied().unwrap_or_default();
    let peak_residue = residues.iter().copied().max().unwrap_or_default();
    println!(
        "{}",
        serde_json::json!({
            "label": "changed_file_sweep",
            "files": files.len(),
            "cycles": cycles,
            "restarts_by_cause": causes,
            "peak_residue_mib": peak_residue / (1024 * 1024),
        })
    );
    assert!(
        cycles < u64::try_from(files.len()).expect("small count"),
        "recycling once per file means the budget is below one import's residue; cycles={cycles}"
    );
}

/// The recycle must reach the child before the Lean heap ceiling aborts it.
///
/// These are two different bounds on the same growth, and only one of them is
/// graceful: crossing the residue budget cycles the worker between calls,
/// crossing the heap ceiling throws a C++ exception that cannot unwind through
/// the Rust frame at the shim boundary and takes the process with it. A fixed
/// 8 GiB ceiling under a 9 GiB budget floor put them in the wrong order, and the
/// sweep above aborted on its third file.
///
/// The default budget needs about six Mathlib-scale imports to reach, which no
/// developer machine survives, so this drives the same ordering at a budget the
/// fixture can actually cross. What it pins is the *cause*: a planned
/// `import_residue` cycle, never `child_abort`.
#[tokio::test]
#[ignore = "manual KanProofs ordering check; multi-GB and minutes long; set LEAN_HOST_MCP_KANPROOFS_EVAL"]
async fn the_residue_recycle_reaches_the_child_before_the_heap_ceiling_aborts_it() {
    use lean_host_mcp::tools::declaration_inventory::{
        DeclarationInventoryRequest, DeclarationInventoryTarget, declaration_inventory,
    };

    let Some(root) = kanproofs_root() else {
        panic!("LEAN_HOST_MCP_KANPROOFS_EVAL not set");
    };
    // Under one import per file at Mathlib scale, so the fourth call is the one
    // that finds the accumulator over budget and cycles before serving.
    let budget_bytes = 3000 * 1024 * 1024;
    let files = [
        "KanProofs/Data/Rat/Lemmas.lean",
        "KanProofs/Data/Int/Factorization.lean",
        "KanProofs/Algebra/Category/ContinuousCohomology/Kummer/RootsOfUnity.lean",
        "KanProofs/Topology/Algebra/Group/ClosedSubgroup.lean",
    ];
    let broker = ProjectBroker::new_with_runtime_config(
        BrokerConfig {
            config_default: None,
            env_default: Some(root.clone()),
            cwd: root,
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: BrokerConfig::default_idle_timeout(),
        },
        ProjectRuntimeConfig::default().with_import_residue_budget_bytes(budget_bytes),
    );
    let ctx = ToolContext {
        broker,
        config: ToolConfig {
            verbosity: TelemetryVerbosity::Full,
            ..ToolConfig::default()
        },
    };

    let mut causes = std::collections::BTreeMap::new();
    for file in files {
        let response = declaration_inventory(
            &ctx,
            DeclarationInventoryRequest {
                target: DeclarationInventoryTarget::File {
                    path: PathBuf::from(file),
                },
                project: None,
                limit: None,
            },
        )
        .await
        .expect("declaration_inventory");
        let result = response
            .result_ref()
            .unwrap_or_else(|| panic!("{file} must be answered"));
        assert_eq!(result.status, "ok", "{file} reported {}", result.status);
        let runtime = response
            .telemetry
            .as_ref()
            .expect("full verbosity keeps telemetry")
            .runtime
            .as_ref()
            .expect("full verbosity keeps runtime facts");
        causes.clone_from(&runtime.restarts_by_cause);
        println!(
            "{}",
            serde_json::json!({
                "file": file,
                "generation": runtime.worker_generation,
                "call_restart": runtime.call_restart.as_ref().map(|event| event.cause.clone()),
                "import_residue_mib": runtime.import_residue_bytes.unwrap_or_default() / (1024 * 1024),
                "import_residue_limit_mib": runtime.import_residue_limit_bytes.unwrap_or_default() / (1024 * 1024),
            })
        );
    }

    println!(
        "{}",
        serde_json::json!({ "label": "ordering", "restarts_by_cause": causes })
    );
    assert!(
        !causes.contains_key("child_abort"),
        "the heap ceiling aborted the child before the budget could recycle it: {causes:?}"
    );
    assert!(
        causes.contains_key("import_residue") || causes.contains_key("import_residue_idle"),
        "a budget this far below the sweep's residue must have cycled at least once: {causes:?}"
    );
}
