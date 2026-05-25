//! Lifecycle smoke test for [`LeanProject`]: open against a real fixture,
//! submit one trivial closure, call `shutdown()`, and confirm the actor
//! thread stops accepting work. Gated on `LEAN_HOST_MCP_TEST_FIXTURE` for
//! the same reason as `tests/worker.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use lean_host_mcp::{LakeProjectMeta, LeanProject, ServerError, default_cache_dir};

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn open_submit_shutdown_round_trip() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let meta = LakeProjectMeta::from_cli(&root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("meta");
    let project = LeanProject::open(meta, &default_cache_dir()).expect("open");

    // Trivial closure: just confirm we can dispatch work to the actor and
    // get a Send + 'static value back.
    let toolchain: String = project
        .submit(|cap| Ok(cap.runtime_metadata().lean_version.unwrap_or_default()))
        .await
        .expect("submit");
    assert!(!toolchain.is_empty(), "fixture worker must report a lean_version");

    project.shutdown();

    // After shutdown, submits return SessionGone. Give the closed-channel
    // path a moment to propagate.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let post = project.submit(|_cap| Ok(())).await;
    assert!(
        matches!(post, Err(ServerError::SessionGone)),
        "submit after shutdown must return SessionGone; got {post:?}"
    );
}
