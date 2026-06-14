//! Warm-session worker round-trip cost. Pins per-request declaration
//! inspection time against the public proof-agent worker path.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (same shape as the e2e env var):
//! the bench is a no-op when unset so `cargo bench` still runs in CI.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::tools::position::ProofPositionSelector;
use lean_host_mcp::tools::proof_action::{TryProofStepRequest, try_proof_step};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProofAttemptEnvelope, ProofAttemptResult};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn bench_worker_roundtrip(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping worker_roundtrip; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
        semantic_permits: BrokerConfig::default_semantic_permits(),
        semantic_waiters: BrokerConfig::default_semantic_waiters(),
        semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
        semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
    });
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };
    // Prime the import set so the first measured iteration doesn't pay the
    // module load cost.
    rt.block_on(async {
        drop(
            inspect_declaration(
                &ctx,
                InspectDeclarationRequest {
                    name: "Nat.add_zero".to_owned(),
                    file: None,
                    imports: vec!["LeanRsFixture.Handles".to_owned()],
                    project: None,
                    fields: InspectDeclarationFields::default(),
                    raw_statement: false,
                },
            )
            .await,
        );
    });

    c.bench_function("worker_roundtrip/inspect_declaration", |b| {
        b.iter(|| {
            rt.block_on(async {
                inspect_declaration(
                    &ctx,
                    InspectDeclarationRequest {
                        name: "Nat.add_zero".to_owned(),
                        file: None,
                        imports: vec!["LeanRsFixture.Handles".to_owned()],
                        project: None,
                        fields: InspectDeclarationFields::default(),
                        raw_statement: false,
                    },
                )
                .await
                .expect("inspect_declaration")
            });
        });
    });

    for candidate_count in [1_usize, 3, 10] {
        let request = proof_step_request(candidate_count);
        let probe = rt
            .block_on(async { try_proof_step(&ctx, request.clone()).await })
            .expect("proof_step probe");
        eprintln!(
            "proof_step_batch_probe count={candidate_count} bytes={} statuses={:?}",
            serde_json::to_vec(&probe).expect("serialize probe response").len(),
            proof_attempt_status_counts(proof_attempt_envelope(
                probe.result.as_ref().expect("proof_step probe result")
            ))
        );

        c.bench_function(&format!("worker_roundtrip/proof_step_batch/{candidate_count}"), |b| {
            b.iter(|| {
                rt.block_on(async {
                    std::hint::black_box(
                        try_proof_step(&ctx, proof_step_request(candidate_count))
                            .await
                            .expect("try_proof_step"),
                    )
                });
            });
        });
    }
}

fn proof_step_request(candidate_count: usize) -> TryProofStepRequest {
    let snippets = (0..candidate_count).map(|_| "trivial".to_owned()).collect();
    TryProofStepRequest {
        file: PathBuf::from("LeanRsFixture/ProofActions.lean"),
        declaration: "LeanRsFixture.ProofActions.stepTheorem".to_owned(),
        proof_position: ProofPositionSelector::Default,
        project: None,
        snippet: None,
        snippets,
    }
}

fn proof_attempt_envelope(result: &ProofAttemptResult) -> &ProofAttemptEnvelope {
    match result {
        ProofAttemptResult::Ok { result, .. } | ProofAttemptResult::MissingImports { result, .. } => result,
        other @ (ProofAttemptResult::HeaderParseFailed { .. } | ProofAttemptResult::Unsupported) => {
            panic!("expected proof-attempt envelope, got {other:?}")
        }
    }
}

fn proof_attempt_status_counts(envelope: &ProofAttemptEnvelope) -> std::collections::BTreeMap<&str, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for candidate in &envelope.candidates {
        let count = counts.entry(candidate.status.as_str()).or_insert(0_usize);
        *count = count.saturating_add(1);
    }
    counts
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = bench_worker_roundtrip
}
criterion_main!(benches);
