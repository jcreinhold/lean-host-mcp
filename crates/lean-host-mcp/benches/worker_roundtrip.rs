//! Warm-session worker round-trip cost. Pins per-request declaration
//! inspection time against the public proof-agent worker path.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (same shape as the e2e env var):
//! the bench is a no-op when unset so `cargo bench` still runs in CI.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::{BrokerConfig, ProjectBroker, default_cache_dir};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn bench_worker_roundtrip(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping worker_roundtrip; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        cache_dir: default_cache_dir(),
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
        semantic_permits: BrokerConfig::default_semantic_permits(),
    });
    let ctx = ToolContext { broker };
    // Prime the import set so the first measured iteration doesn't pay the
    // module load cost.
    rt.block_on(async {
        drop(
            inspect_declaration(
                &ctx,
                InspectDeclarationRequest {
                    name: "Nat.add_zero".to_owned(),
                    file: None,
                    imports: vec!["LeanRsFixture.Handles".to_owned()],
                    project: None,
                    fields: InspectDeclarationFields::default(),
                    max_field_bytes: Some(512),
                    max_total_bytes: Some(2048),
                },
            )
            .await,
        );
    });

    c.bench_function("worker_roundtrip/inspect_declaration", |b| {
        b.iter(|| {
            rt.block_on(async {
                inspect_declaration(
                    &ctx,
                    InspectDeclarationRequest {
                        name: "Nat.add_zero".to_owned(),
                        file: None,
                        imports: vec!["LeanRsFixture.Handles".to_owned()],
                        project: None,
                        fields: InspectDeclarationFields::default(),
                        max_field_bytes: Some(512),
                        max_total_bytes: Some(2048),
                    },
                )
                .await
                .expect("inspect_declaration")
            });
        });
    });
}

criterion_group!(benches, bench_worker_roundtrip);
criterion_main!(benches);
