//! Broker dispatch cost across the three hot paths the multi-project pool
//! introduces:
//!
//! - **warm**: project already resident; cost is mutex acquisition, LRU
//!   promote, manifest SHA-256, `Arc::clone`. Target: < 50 µs at p99.
//! - **cold**: pool empty; cost is [`LeanProject::open`] + insert. Dominated
//!   by worker-child spawn (multi-second).
//! - **eviction**: pool at capacity (1), alternating two projects. Cost is
//!   shutdown of the LRU victim plus a cold open of the requested project.
//!
//! Gated on `LEAN_HOST_MCP_BENCH_FIXTURE` (falls back to
//! `LEAN_HOST_MCP_TEST_FIXTURE`). The eviction case needs a second project
//! root; we synthesize one from the fixture by copying the lakefile,
//! toolchain pin, and manifest into a tempdir and symlinking `.lake/`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint, default_cache_dir};
use tokio::runtime::Runtime;

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_BENCH_FIXTURE")
        .or_else(|_| std::env::var("LEAN_HOST_MCP_TEST_FIXTURE"))
        .ok()
        .map(PathBuf::from)
}

fn make_synthetic_project(fixture_root: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("synth tempdir");
    let synth = dir.path();
    for file in ["lakefile.toml", "lakefile.lean", "lean-toolchain", "lake-manifest.json"] {
        let src = fixture_root.join(file);
        if src.exists() {
            std::fs::copy(&src, synth.join(file)).expect("copy fixture file");
        }
    }
    let lake_src = fixture_root.join(".lake");
    if lake_src.exists() {
        std::os::unix::fs::symlink(&lake_src, synth.join(".lake")).expect("symlink .lake");
    }
    let canon = synth.canonicalize().expect("canonicalise synth");
    (dir, canon)
}

fn make_broker(env_default: Option<PathBuf>, max_projects: NonZeroUsize) -> Arc<ProjectBroker> {
    let cwd = env_default.clone().unwrap_or_else(|| PathBuf::from("/"));
    ProjectBroker::new(BrokerConfig {
        cache_dir: default_cache_dir(),
        config_default: None,
        env_default,
        cwd,
        max_projects,
        idle_timeout: Duration::ZERO,
    })
}

async fn touch(broker: &Arc<ProjectBroker>, hint: ProjectHint) {
    broker
        .with_project(hint, |_project| async move { Ok(()) })
        .await
        .expect("with_project");
}

fn bench_warm(c: &mut Criterion, rt: &Runtime, root: &Path) {
    let broker = make_broker(Some(root.to_path_buf()), NonZeroUsize::new(4).unwrap());
    // Prime: open the project once so subsequent iterations are warm.
    rt.block_on(touch(&broker, ProjectHint::Default));

    c.bench_function("multi_project_dispatch/warm", |b| {
        b.iter(|| {
            rt.block_on(touch(&broker, ProjectHint::Default));
        });
    });
}

fn bench_cold(c: &mut Criterion, rt: &Runtime, root: &Path) {
    let mut group = c.benchmark_group("multi_project_dispatch/cold");
    group.sample_size(10);
    group.bench_function("first_open", |b| {
        b.iter(|| {
            let broker = make_broker(Some(root.to_path_buf()), NonZeroUsize::new(4).unwrap());
            rt.block_on(touch(&broker, ProjectHint::Default));
        });
    });
    group.finish();
}

fn bench_eviction(c: &mut Criterion, rt: &Runtime, root: &Path) {
    let (_synth_keep, synth_root) = make_synthetic_project(root);
    let broker = make_broker(Some(root.to_path_buf()), NonZeroUsize::new(1).unwrap());
    // Prime: project A resident.
    rt.block_on(touch(&broker, ProjectHint::Default));

    let mut group = c.benchmark_group("multi_project_dispatch/eviction");
    group.sample_size(10);
    group.bench_function("alternate_two_projects", |b| {
        b.iter(|| {
            rt.block_on(touch(&broker, ProjectHint::Explicit(synth_root.clone())));
            rt.block_on(touch(&broker, ProjectHint::Default));
        });
    });
    group.finish();
}

fn bench_multi_project_dispatch(c: &mut Criterion) {
    let Some(root) = fixture_root() else {
        eprintln!("skipping multi_project_dispatch; set LEAN_HOST_MCP_BENCH_FIXTURE");
        return;
    };
    let canonical = root.canonicalize().expect("canonicalise fixture");
    let rt = Runtime::new().unwrap();
    bench_warm(c, &rt, &canonical);
    bench_cold(c, &rt, &canonical);
    bench_eviction(c, &rt, &canonical);
}

criterion_group!(benches, bench_multi_project_dispatch);
criterion_main!(benches);
