//! Warm `search_for_proof` cost — the most expensive tool call in the surface.
//!
//! `search_for_proof` is the only tool that touches *two* worker children: the
//! elaboration worker for the proof state, and a second semantic-search child
//! for ranking. That second child used to be built and torn down on every call,
//! along with a re-run of `lean_semantic_search_runtime::build_cached` (an
//! on-disk-cached Lake build, ~5.5 ms warm). Both are now resident, and this
//! bench is what defends that: it measures the *repeat* call, where the
//! difference is the entire per-call setup rather than a fraction of it.
//!
//! The first call is deliberately outside the measurement — spawning the two
//! children and importing once is startup, not steady state, and leaving it in
//! would bury the thing being measured.
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
use lean_host_mcp::tools::position::ProofPositionSelector;
use lean_host_mcp::tools::proof_search::{ProofSearchMode, SearchForProofRequest, search_for_proof};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn request() -> SearchForProofRequest {
    SearchForProofRequest {
        file: Some(PathBuf::from("LeanRsFixture/ProofAgent.lean")),
        declaration: Some("LeanRsFixture.ProofAgent.miniRatDenominatorStep".to_owned()),
        proof_position: ProofPositionSelector::default(),
        goal: None,
        type_text: None,
        imports: Vec::new(),
        mode: Some(ProofSearchMode::NextStep),
        limit: Some(10),
        project: None,
    }
}

fn bench_search_for_proof(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping search_for_proof; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };

    // Spawn both children and import once, outside the measurement.
    rt.block_on(async { drop(search_for_proof(&ctx, request()).await) });

    let mut group = c.benchmark_group("search_for_proof");
    // A single call elaborates a declaration and runs semantic ranking, so the
    // criterion defaults would spend minutes here for no extra resolution.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("warm_next_step", |b| {
        b.iter(|| {
            rt.block_on(async {
                search_for_proof(&ctx, request()).await.expect("search_for_proof");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_search_for_proof);
criterion_main!(benches);
