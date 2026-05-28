//! Cold spawn cost: `LeanProject::open` + first `infer_type`. Recorded for
//! visibility; no hard threshold (cold spawn is one-off per server start).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::lean::{InferTypeRequest, infer_type};
use lean_host_mcp::{BrokerConfig, ProjectBroker, default_cache_dir};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn bench_worker_cold_spawn(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping worker_cold_spawn; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("worker_cold_spawn");
    group.sample_size(10);
    group.bench_function("spawn_plus_first_infer", |b| {
        b.iter(|| {
            let imports = vec!["LeanRsFixture.Handles".to_owned()];
            // Fresh broker per iteration so each one pays the project open
            // cost (the point of this bench).
            let broker = ProjectBroker::new(BrokerConfig {
                cache_dir: default_cache_dir(),
                config_default: None,
                env_default: Some(root.clone()),
                cwd: root.clone(),
                max_projects: BrokerConfig::default_max_projects(),
                idle_timeout: BrokerConfig::default_idle_timeout(),
                semantic_permits: BrokerConfig::default_semantic_permits(),
            });
            let ctx = ToolContext { broker };
            rt.block_on(async {
                infer_type(
                    &ctx,
                    InferTypeRequest {
                        term: "Nat.succ Nat.zero".into(),
                        imports,
                        project: None,
                    },
                )
                .await
                .expect("infer");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_worker_cold_spawn);
criterion_main!(benches);
