//! Warm-session worker round-trip cost. Pins per-request `open_session +
//! infer_type` time against [`SessionHost`]. Target: ≤ 2 ms per call.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (same shape as the e2e env var):
//! the bench is a no-op when unset so `cargo bench` still runs in CI.

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

fn bench_worker_roundtrip(c: &mut Criterion) {
    let Some((root, pkg, lib)) = fixture_env() else {
        eprintln!("skipping worker_roundtrip; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let imports = vec!["LeanRsFixture.Handles".to_owned()];
    let host = SessionHost::spawn(root, pkg, lib, imports.clone()).expect("spawn");
    // Prime the import set so the first measured iteration doesn't pay the
    // module load cost.
    rt.block_on(async {
        drop(host.infer_type("Nat.succ Nat.zero".into(), imports.clone()).await);
    });

    c.bench_function("worker_roundtrip/infer_type", |b| {
        b.iter(|| {
            rt.block_on(async {
                host.infer_type("Nat.succ Nat.zero".into(), imports.clone())
                    .await
                    .expect("infer_type")
            });
        });
    });
}

criterion_group!(benches, bench_worker_roundtrip);
criterion_main!(benches);
