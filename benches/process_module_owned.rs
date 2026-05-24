//! Process-module overhead vs. raw Lean elaboration. Target: projection +
//! transport overhead under 20% of Lean elaboration time for the same file.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::SessionHost;
use tokio::runtime::Runtime;

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

fn bench_process_module(c: &mut Criterion) {
    let Some((root, pkg, lib)) = fixture_env() else {
        eprintln!("skipping process_module_owned; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let imports = vec!["LeanRsFixture.Handles".to_owned()];
    let host = SessionHost::spawn(root.clone(), pkg, lib, imports).expect("spawn");
    let source = std::fs::read_to_string(root.join("LeanRsFixture/SourceRanges.lean")).expect("read fixture");

    let mut group = c.benchmark_group("process_module_owned");
    group.sample_size(20);
    group.bench_function("source_ranges", |b| {
        b.iter(|| {
            rt.block_on(async {
                host.process_module(source.clone()).await.expect("process_module");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_process_module);
criterion_main!(benches);
