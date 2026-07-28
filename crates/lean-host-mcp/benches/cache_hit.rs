//! What a warm `proof_state` on an unmodified file actually costs.
//!
//! `ModuleQueryCache` exists so that repeating a position query against a file
//! nobody has edited never reaches the worker: the lookup is an in-process map
//! probe on the project handle, and a hit answers without entering the actor's
//! mailbox at all. This is the bench that holds that claim to a number.
//!
//! Nothing else covered it. `module_query_roundtrip` calls the *uncached*
//! `process_module_query` entry point and measures 18–20 ms per query — a full
//! worker round trip — so it says nothing about a hit. `multi_project_dispatch`
//! measures broker dispatch (its `warm` arm is where this file's `< 50 µs at
//! p99` reference target comes from) but never issues a repeat query.
//!
//! Two arms against one resident project, differing only in whether the cache
//! can answer:
//!
//! - `warm_repeat` asks the identical question about an unmodified file, so
//!   every iteration after the first is a hit.
//! - `content_changed` appends a distinct comment per iteration, changing the
//!   content hash in the key and forcing a miss — the elaboration cost the hit
//!   is avoiding. It is *not* a regression when this arm is slow; it is the
//!   baseline the first arm should be orders of magnitude below.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (falling back to
//! `LEAN_HOST_MCP_TEST_FIXTURE`): a no-op when unset, so `cargo bench` still
//! runs in CI.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::position::{ProofStateRequest, proof_state};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ResponseStatus};
use tokio::runtime::Runtime;

const FIXTURE_FILE: &str = "LeanRsFixture/SourceRanges.lean";
const FIXTURE_DECLARATION: &str = "LeanRsFixture.SourceRanges.knownTheorem";

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn request(file: PathBuf) -> ProofStateRequest {
    ProofStateRequest {
        file,
        declaration: FIXTURE_DECLARATION.to_owned(),
        proof_position: lean_host_mcp::tools::position::ProofPositionSelector::default(),
        include_boundaries: false,
        include_expected_type: false,
        project: None,
    }
}

async fn ask(ctx: &ToolContext, file: PathBuf) {
    let response = proof_state(ctx, request(file)).await.expect("proof_state");
    // Guard against timing a fast failure: a `runtime_unavailable` envelope
    // would measure an error path, not a cache hit.
    assert!(
        matches!(response.status, ResponseStatus::Ok),
        "bench arm must produce a real answer"
    );
}

fn bench_cache_hit(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping cache_hit; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root.clone(),
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };
    let unmodified = root.join(FIXTURE_FILE);

    // Open the project, spawn the worker, and fill the cache entry, all outside
    // the measurement.
    rt.block_on(ask(&ctx, unmodified.clone()));

    let mut group = c.benchmark_group("cache_hit");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("warm_repeat", |b| {
        b.iter(|| {
            rt.block_on(ask(&ctx, unmodified.clone()));
        });
    });

    // A scratch copy so the miss arm never edits the fixture in the repository.
    let scratch_dir = tempfile::tempdir().expect("scratch dir for the miss arm");
    let scratch = scratch_dir.path().join("CacheHitMiss.lean");
    let original = std::fs::read_to_string(&unmodified).expect("read fixture source");

    group.bench_function("content_changed", |b| {
        // `Cell` rather than a captured `mut`: `iter` takes `FnMut`, and the
        // counter must advance across calls for each iteration to be a miss.
        let iteration = Cell::new(0_u64);
        b.iter(|| {
            let n = iteration.replace(iteration.get().saturating_add(1));
            // A trailing comment changes the content hash without changing a
            // single elaborated declaration, so the two arms ask the same
            // question of Lean and differ only in cacheability.
            std::fs::write(&scratch, format!("{original}\n-- cache_hit miss {n}\n")).expect("write scratch source");
            rt.block_on(ask(&ctx, scratch.clone()));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_cache_hit);
criterion_main!(benches);
