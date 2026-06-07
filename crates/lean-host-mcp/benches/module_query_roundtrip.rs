//! Bounded module-query round trips for the position-tool workload.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint};
use lean_rs_worker_parent::{LeanWorkerElabOptions, LeanWorkerModuleQuery};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn bench_module_query(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping module_query_roundtrip; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let source = std::fs::read_to_string(root.join("LeanRsFixture/SourceRanges.lean")).expect("read fixture");

    let queries = [
        ("diagnostics", LeanWorkerModuleQuery::Diagnostics),
        ("type_at", LeanWorkerModuleQuery::TypeAt { line: 3, column: 10 }),
        ("goal_at", LeanWorkerModuleQuery::GoalAt { line: 3, column: 10 }),
        (
            "references",
            LeanWorkerModuleQuery::References {
                name: "LeanRsFixture.sourceRangeTarget".to_owned(),
            },
        ),
    ];

    let mut group = c.benchmark_group("module_query_roundtrip");
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(5));
    for (name, query) in queries {
        let rt = Runtime::new().unwrap();
        let imports = vec!["LeanRsFixture.Handles".to_owned()];
        let broker = ProjectBroker::new(BrokerConfig {
            config_default: None,
            env_default: Some(root.clone()),
            cwd: root.clone(),
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: BrokerConfig::default_idle_timeout(),
            semantic_permits: BrokerConfig::default_semantic_permits(),
            semantic_waiters: BrokerConfig::default_semantic_waiters(),
            semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
            semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
        });
        group.bench_function(name, |b| {
            b.iter(|| {
                let source = source.clone();
                let imports = imports.clone();
                let query = query.clone();
                rt.block_on(async {
                    broker
                        .process_module_query(
                            ProjectHint::Default,
                            imports.clone(),
                            imports,
                            source,
                            query,
                            LeanWorkerElabOptions::new(),
                        )
                        .await
                        .expect("process_module_query");
                });
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_module_query);
criterion_main!(benches);
