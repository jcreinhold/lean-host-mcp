//! Multi-project broker behavior: coexistence, LRU eviction, idle
//! eviction, manifest invalidation. Gated on `LEAN_HOST_MCP_TEST_FIXTURE`
//! pointing at any *built* Lake project (the `lean-rs-host` shims are
//! bundled in `lean-rs-host`; consumers don't link them).
//!
//! Tests use the project's `session_id` (stamped into every
//! [`Freshness`](lean_host_mcp::Freshness) envelope) as the identity
//! signal: the broker re-allocates `session_id` on every successful
//! [`LeanProject::open`], so a value change between two
//! [`ProjectBroker::with_project`] calls means the underlying actor was
//! shut down and replaced.
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

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

fn make_broker(env_default: Option<PathBuf>, max_projects: NonZeroUsize, idle_timeout: Duration) -> Arc<ProjectBroker> {
    let cache_dir = tempfile::tempdir().expect("cache tempdir").keep();
    let cwd = env_default.clone().unwrap_or_else(|| PathBuf::from("/"));
    ProjectBroker::new(BrokerConfig {
        cache_dir,
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
        .with_project(hint, |project| async move { Ok(project.session_id().to_owned()) })
        .await
        .expect("with_project")
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn two_projects_coexist_in_pool() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let canonical_root = root.canonicalize().expect("canonicalise fixture");
    let (_synth_keep, synth_root) = make_synthetic_project(&canonical_root);
    let broker = make_broker(Some(canonical_root.clone()), NonZeroUsize::new(2).unwrap(), Duration::ZERO);

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
    let broker = make_broker(Some(canonical_root.clone()), NonZeroUsize::new(1).unwrap(), Duration::ZERO);

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
    assert_ne!(
        id_first, id_second,
        "post-reaper request must re-spawn the project"
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
