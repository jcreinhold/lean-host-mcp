//! Cold spawn cost: project open + first declaration inspection. Recorded
//! for visibility; no hard threshold (cold spawn is one-off per server start).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::{BrokerConfig, ProjectBroker};
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
    group.bench_function("spawn_plus_first_inspect", |b| {
        b.iter(|| {
            // Fresh broker per iteration so each one pays the project open
            // cost (the point of this bench).
            let broker = ProjectBroker::new(BrokerConfig {
                config_default: None,
                env_default: Some(root.clone()),
                cwd: root.clone(),
                max_projects: BrokerConfig::default_max_projects(),
                idle_timeout: BrokerConfig::default_idle_timeout(),
                semantic_permits: BrokerConfig::default_semantic_permits(),
                semantic_waiters: BrokerConfig::default_semantic_waiters(),
                semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
            });
            let ctx = ToolContext { broker, config: ToolConfig::default() };
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
                .expect("inspect_declaration");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_worker_cold_spawn);
criterion_main!(benches);
