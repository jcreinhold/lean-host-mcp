//! Cold spawn cost: `SessionHost::spawn` + first `infer_type`. Recorded for
//! visibility; no hard threshold (cold spawn is one-off per server start).

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

fn bench_worker_cold_spawn(c: &mut Criterion) {
    let Some((root, pkg, lib)) = fixture_env() else {
        eprintln!("skipping worker_cold_spawn; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("worker_cold_spawn");
    group.sample_size(10);
    group.bench_function("spawn_plus_first_infer", |b| {
        b.iter(|| {
            let imports = vec!["LeanRsFixture.Handles".to_owned()];
            let host = SessionHost::spawn(root.clone(), pkg.clone(), lib.clone(), imports.clone()).expect("spawn");
            rt.block_on(async {
                host.infer_type("Nat.succ Nat.zero".into(), imports)
                    .await
                    .expect("infer");
            });
        });
    });
    group.finish();
}

criterion_group!(benches, bench_worker_cold_spawn);
criterion_main!(benches);
