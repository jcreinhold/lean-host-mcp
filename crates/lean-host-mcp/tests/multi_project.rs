//! Multi-project broker behavior: coexistence, LRU eviction, idle
//! eviction, manifest invalidation. Gated on `LEAN_HOST_MCP_TEST_FIXTURE`
//! pointing at any *built* Lake project (the `lean-rs-host` shims are
//! bundled in `lean-rs-host`; consumers don't link them).
//!
//! Tests use the project's `session_id` (stamped into every
//! [`Freshness`](lean_host_mcp::Freshness) envelope) as the identity
//! signal: the broker re-allocates `session_id` on every successful
//! opening a private project runtime, so a value change between two
//! [`ProjectBroker::admitted_project_runtime`] calls means the underlying
//! controller was shut down and replaced.
//!
//! A second "project" is synthesized from the real fixture: a tempdir
//! containing the four files [`LakeProjectMeta::from_explicit`] reads
//! (`lakefile.{toml,lean}`, `lean-toolchain`, `lake-manifest.json`) plus a
//! symlink to the fixture's `.lake/` so the worker preflight finds its
//! `.olean` files. Tests in this file never submit real Lean work to the
//! synthetic project; they exercise broker dispatch and lifecycle only.
//!
//! ```sh
//! cd /path/to/lean-host-mcp/fixtures/lean && lake build
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
//!     cargo test --test multi_project -- --ignored
//! ```

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn make_broker(env_default: Option<PathBuf>, max_projects: NonZeroUsize, idle_timeout: Duration) -> Arc<ProjectBroker> {
    let cwd = env_default.clone().unwrap_or_else(|| PathBuf::from("/"));
    ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default,
        cwd,
        max_projects,
        idle_timeout,
    })
}

/// Build a synthetic Lake-project root that shares the fixture's build
/// artifacts. Returns the canonicalised root; the [`tempfile::TempDir`] is
/// kept (returned) so the caller can keep it alive for the test's duration.
fn make_synthetic_project(fixture_root: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("synth tempdir");
    let synth = dir.path();
    for file in ["lakefile.toml", "lakefile.lean", "lean-toolchain", "lake-manifest.json"] {
        let src = fixture_root.join(file);
        if src.exists() {
            std::fs::copy(&src, synth.join(file)).expect("copy fixture file");
        }
    }
    // Symlink .lake/ so the worker preflight resolves .olean files
    // against the fixture's existing build without us re-running `lake build`.
    let lake_src = fixture_root.join(".lake");
    if lake_src.exists() {
        std::os::unix::fs::symlink(&lake_src, synth.join(".lake")).expect("symlink .lake");
    }
    let canon = synth.canonicalize().expect("canonicalise synth");
    (dir, canon)
}

async fn session_id_for(broker: &Arc<ProjectBroker>, hint: ProjectHint) -> String {
    broker
        .admitted_project_runtime(hint, Vec::new())
        .await
        .expect("admitted_project_runtime")
        .freshness
        .session_id
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn two_projects_coexist_in_pool() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let (_synth_keep, synth_root) = make_synthetic_project(&canonical_root);
    let broker = make_broker(
        Some(canonical_root.clone()),
        NonZeroUsize::new(2).unwrap(),
        Duration::ZERO,
    );

    let id_default_first = session_id_for(&broker, ProjectHint::Default).await;
    let id_explicit_first = session_id_for(&broker, ProjectHint::Explicit(synth_root.clone())).await;
    assert_ne!(
        id_default_first, id_explicit_first,
        "two distinct projects must have distinct session_id values"
    );

    // Second pass against each: both must still be resident (no eviction).
    let id_default_second = session_id_for(&broker, ProjectHint::Default).await;
    let id_explicit_second = session_id_for(&broker, ProjectHint::Explicit(synth_root.clone())).await;
    assert_eq!(
        id_default_first, id_default_second,
        "default project must stay resident across calls"
    );
    assert_eq!(
        id_explicit_first, id_explicit_second,
        "explicit project must stay resident across calls"
    );

    let resident = broker.resident_paths();
    assert!(
        resident.contains(&canonical_root) && resident.contains(&synth_root),
        "both projects must be resident; got {resident:?}"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn lru_eviction_respawns_evicted_project() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let (_synth_keep, synth_root) = make_synthetic_project(&canonical_root);
    let broker = make_broker(
        Some(canonical_root.clone()),
        NonZeroUsize::new(1).unwrap(),
        Duration::ZERO,
    );

    let id_a_first = session_id_for(&broker, ProjectHint::Default).await;
    // Touching B with capacity 1 must evict A.
    let _id_b = session_id_for(&broker, ProjectHint::Explicit(synth_root.clone())).await;
    // Touching A again must re-spawn it; session_id changes.
    let id_a_second = session_id_for(&broker, ProjectHint::Default).await;
    assert_ne!(
        id_a_first, id_a_second,
        "evicted-then-rerequested project must have a fresh session_id"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn idle_reaper_evicts_stale_project() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    // Idle window is 1 ms: the project is eligible for reaping as soon as
    // we step past it. Tests don't wait for the 60 s background tick; they
    // call reap_idle() directly.
    let broker = make_broker(
        Some(canonical_root.clone()),
        NonZeroUsize::new(4).unwrap(),
        Duration::from_millis(1),
    );

    let id_first = session_id_for(&broker, ProjectHint::Default).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    broker.reap_idle();
    assert!(
        broker.resident_paths().is_empty(),
        "idle reaper must have evicted the only resident project"
    );

    let id_second = session_id_for(&broker, ProjectHint::Default).await;
    assert_ne!(id_first, id_second, "post-reaper request must re-spawn the project");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn shutdown_all_evicts_resident_projects() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let broker = make_broker(Some(canonical_root), NonZeroUsize::new(4).unwrap(), Duration::ZERO);

    let _id_first = session_id_for(&broker, ProjectHint::Default).await;
    assert!(!broker.resident_paths().is_empty(), "project should be resident");

    broker.shutdown_all();
    assert!(
        broker.resident_paths().is_empty(),
        "shutdown_all must clear resident projects"
    );

    let id_after_shutdown = session_id_for(&broker, ProjectHint::Default).await;
    assert!(
        !id_after_shutdown.is_empty(),
        "broker should reopen a project after explicit shutdown"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn manifest_mutation_triggers_respawn() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let (_synth_keep, synth_root) = make_synthetic_project(&canonical_root);
    let broker = make_broker(None, NonZeroUsize::new(2).unwrap(), Duration::ZERO);

    let id_first = session_id_for(&broker, ProjectHint::Explicit(synth_root.clone())).await;

    // Mutate the synthetic project's lake-manifest.json so its SHA-256
    // shifts. We append a byte rather than rewriting to keep the JSON
    // shape vaguely intact (the broker only hashes, doesn't parse).
    let manifest = synth_root.join("lake-manifest.json");
    let mut bytes = std::fs::read(&manifest).expect("read manifest");
    bytes.push(b'\n');
    std::fs::write(&manifest, &bytes).expect("write manifest");

    let id_second = session_id_for(&broker, ProjectHint::Explicit(synth_root)).await;
    assert_ne!(
        id_first, id_second,
        "manifest mutation must invalidate the cached project and re-spawn"
    );
}

/// Two projects are two workers, so calls against them run at the same time.
///
/// This is the contract that replaced the process-wide semantic-admission
/// semaphore. With one global permit, a call to project B waited for project
/// A's unrelated call to finish; the only thing that distinguishes the two
/// regimes is wall clock, so the assertion has to be a wall-clock one. It is
/// framed as an A/B against the *same* workload measured serially rather than
/// against an absolute latency budget: a slow or loaded machine inflates both
/// arms, so the ratio stays meaningful where a fixed millisecond bound would
/// not. Both projects are warmed first so worker bootstrap is outside both
/// measurements.
#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn concurrent_calls_across_projects_do_not_queue() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let (_synth_keep, synth_root) = make_synthetic_project(&canonical_root);
    let broker = make_broker(None, NonZeroUsize::new(2).unwrap(), Duration::ZERO);

    let a = ProjectHint::Explicit(canonical_root);
    let b = ProjectHint::Explicit(synth_root);

    // Warm both workers; the first call to each pays the worker spawn.
    inspect(&broker, a.clone()).await;
    inspect(&broker, b.clone()).await;

    let serial_start = std::time::Instant::now();
    inspect(&broker, a.clone()).await;
    inspect(&broker, b.clone()).await;
    let serial = serial_start.elapsed();

    let concurrent_start = std::time::Instant::now();
    let (first, second) = tokio::join!(inspect(&broker, a), inspect(&broker, b));
    let concurrent = concurrent_start.elapsed();

    assert!(
        first && second,
        "both projects must have resolved the declaration, or the timing compares nothing"
    );
    assert!(
        concurrent.as_secs_f64() < serial.as_secs_f64() * 0.8,
        "calls to different projects must overlap, not serialize; serial={serial:?} concurrent={concurrent:?}"
    );
}

/// One real (elaborating) worker call against a core declaration, reporting
/// whether it resolved so the caller can confirm the timed arms did real work.
async fn inspect(broker: &Arc<ProjectBroker>, hint: ProjectHint) -> bool {
    use lean_rs_worker_parent::{LeanWorkerDeclarationInspectionRequest, LeanWorkerDeclarationInspectionResult};

    let imports = vec!["Init".to_owned()];
    let call = broker
        .inspect_declaration(
            hint,
            imports.clone(),
            imports,
            LeanWorkerDeclarationInspectionRequest::new("Nat.add_zero"),
        )
        .await
        .expect("inspect_declaration");
    matches!(call.value, LeanWorkerDeclarationInspectionResult::Found { .. })
}
