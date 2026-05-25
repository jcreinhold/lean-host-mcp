//! Process-module overhead vs. raw Lean elaboration. Target: projection +
//! transport overhead under 20% of Lean elaboration time for the same file.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::{LakeProjectMeta, LeanProject, default_cache_dir};
use lean_rs_worker::LeanWorkerElabOptions;
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn bench_process_module(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping process_module_owned; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let imports = vec!["LeanRsFixture.Handles".to_owned()];
    let meta = LakeProjectMeta::from_explicit(&root).expect("meta");
    let project = LeanProject::open(meta, &default_cache_dir()).expect("open");
    let source = std::fs::read_to_string(root.join("LeanRsFixture/SourceRanges.lean")).expect("read fixture");

    let mut group = c.benchmark_group("process_module_owned");
    group.sample_size(20);
    group.bench_function("source_ranges", |b| {
        b.iter(|| {
            let source = source.clone();
            let imports = imports.clone();
            rt.block_on(async {
                project
                    .submit(move |cap| {
                        let mut session = cap
                            .open_session_with_imports(imports, None, None)
                            .map_err(lean_host_mcp::projections::map_worker_err)?;
                        session
                            .process_module(&source, &LeanWorkerElabOptions::new(), None, None)
                            .map_err(lean_host_mcp::projections::map_worker_err)
                    })
                    .await
                    .expect("process_module");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_process_module);
criterion_main!(benches);
