//! What an import-profile switch costs, against one resident project.
//!
//! The gap this fills: `worker_roundtrip` deliberately primes its import set so
//! the measured iterations never re-import, and `module_query_roundtrip` reuses
//! one set throughout. Nothing measured the *switch* — which is what the
//! deleted `IMPORT_SWITCH_RSS_SOFT_KIB` cycle claimed to protect, and what the
//! worker child's session-mismatch path now handles by dropping the old session
//! before importing the replacement.
//!
//! Two arms, same tool, same project, so the difference between them is the
//! import switch and nothing else:
//!
//! - `steady` repeats one import set. The worker child reuses its live session,
//!   so this is the floor: query cost with no import.
//! - `alternating` flips between two disjoint-enough sets every call, forcing a
//!   re-import on each. The ratio to `steady` is the price of a switch.
//!
//! Both use `inspect_declaration` on a core name resolvable under either set,
//! so the *answer* is identical in both arms and only the environment differs.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (same shape as the e2e env var): a
//! no-op when unset, so `cargo bench` still runs in CI.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use lean_host_mcp::tools::{ToolConfig, ToolContext};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ResponseStatus};
use tokio::runtime::Runtime;

/// Resolvable under either import set below, so the two arms differ only in the
/// environment the worker holds, never in the work the query does.
const SHARED_DECLARATION: &str = "Nat.add_zero";

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

async fn inspect(ctx: &ToolContext, imports: Vec<String>) {
    let response = inspect_declaration(
        ctx,
        InspectDeclarationRequest {
            name: SHARED_DECLARATION.to_owned(),
            file: None,
            imports,
            project: None,
            raw_statement: false,
            fields: InspectDeclarationFields::default(),
        },
    )
    .await
    .expect("inspect_declaration");
    // Guard against measuring a fast failure path: a `runtime_unavailable`
    // envelope would time an error, not an import.
    assert!(
        matches!(response.status, ResponseStatus::Ok),
        "bench arm must produce a real answer"
    );
}

fn bench_import_switch(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping import_switch; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let rt = Runtime::new().unwrap();
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    let ctx = ToolContext {
        broker,
        config: ToolConfig::default(),
    };

    let profile_a = vec!["LeanRsFixture.Handles".to_owned()];
    let profile_b = vec!["LeanRsFixture.Strings".to_owned(), "LeanRsFixture.Scalars".to_owned()];

    // Open the project and pay both cold imports once, outside the measurement.
    rt.block_on(async {
        inspect(&ctx, profile_a.clone()).await;
        inspect(&ctx, profile_b.clone()).await;
    });

    let mut group = c.benchmark_group("import_switch");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(20));

    group.bench_function("steady", |b| {
        b.iter(|| {
            rt.block_on(inspect(&ctx, profile_a.clone()));
        });
    });

    group.bench_function("alternating", |b| {
        // `Cell` rather than a captured `mut`: `iter` takes `FnMut`, and the
        // flip must survive across calls for the arm to alternate at all.
        let flip = Cell::new(false);
        b.iter(|| {
            let imports = if flip.replace(!flip.get()) {
                profile_a.clone()
            } else {
                profile_b.clone()
            };
            rt.block_on(inspect(&ctx, imports));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_import_switch);
criterion_main!(benches);
