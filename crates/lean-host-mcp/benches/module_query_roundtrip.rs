//! Bounded module-query round trips for the position-tool workload.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::{LakeProjectMeta, LeanProject, default_cache_dir};
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
        let meta = LakeProjectMeta::from_explicit(&root).expect("meta");
        let project = LeanProject::open(meta, &default_cache_dir()).expect("open");
        group.bench_function(name, |b| {
            b.iter(|| {
                let source = source.clone();
                let imports = imports.clone();
                let query = query.clone();
                rt.block_on(async {
                    project
                        .submit(move |cap| {
                            let mut session = cap
                                .open_session_with_imports(imports, None, None)
                                .map_err(lean_host_mcp::projections::map_worker_err)?;
                            session
                                .process_module_query(&source, query, &LeanWorkerElabOptions::new(), None, None)
                                .map_err(lean_host_mcp::projections::map_worker_err)
                        })
                        .await
                        .expect("process_module_query");
                });
            });
        });
        project.shutdown();
    }
    group.finish();
}

criterion_group!(benches, bench_module_query);
criterion_main!(benches);
