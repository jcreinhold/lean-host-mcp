//! Position lookup after cache warm-up. Target: ≤ 50 µs per query against a
//! freshly-cached `LeanWorkerProcessedFile`. Catches accidental quadratic
//! behavior in the cache helpers.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use std::path::PathBuf;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::SessionHost;
use lean_host_mcp::cache;
use lean_rs_worker::LeanWorkerProcessModuleOutcome;
use tokio::runtime::Runtime;

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

fn bench_position_lookup(c: &mut Criterion) {
    let Some((root, pkg, lib)) = fixture_env() else {
        eprintln!("skipping position_lookup_after_cache_warm; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let imports = vec!["LeanRsFixture.Handles".to_owned()];
    let host = SessionHost::spawn(root.clone(), pkg, lib, imports).expect("spawn");
    let source = std::fs::read_to_string(root.join("LeanRsFixture/SourceRanges.lean")).expect("read fixture");
    let outcome = rt.block_on(host.process_module(source)).expect("process_module");
    let file = match outcome {
        LeanWorkerProcessModuleOutcome::Ok { file, .. }
        | LeanWorkerProcessModuleOutcome::MissingImports { file, .. } => Arc::new(file),
        other => panic!("unexpected outcome for fixture: {other:?}"),
    };

    let probes: Vec<(u32, u32)> = file
        .tactics
        .iter()
        .map(|t| (t.start_line, t.start_column))
        .chain(file.terms.iter().map(|t| (t.start_line, t.start_column)))
        .take(100)
        .collect();
    assert!(!probes.is_empty(), "fixture must record at least one positional probe");

    c.bench_function("position_lookup/tactic_at_then_term_at", |b| {
        b.iter(|| {
            for &(line, col) in &probes {
                let _ = cache::tactic_at(&file, line, col);
                let _ = cache::term_at(&file, line, col);
            }
        });
    });
}

criterion_group!(benches, bench_position_lookup);
criterion_main!(benches);
