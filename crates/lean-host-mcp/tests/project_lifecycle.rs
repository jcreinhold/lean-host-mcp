//! Lifecycle smoke test for [`LeanProject`]: open against a real fixture,
//! call one trivial closure, call `shutdown()`, and confirm the actor
//! thread stops accepting work. Gated on `LEAN_HOST_MCP_TEST_FIXTURE` for
//! the same reason as `tests/worker.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::path::PathBuf;
use std::time::Duration;

use lean_host_mcp::{LakeProjectMeta, LeanProject, ServerError, default_cache_dir};
use lean_rs_worker_parent::{
    LeanWorkerDeclarationInspectionFields, LeanWorkerDeclarationInspectionRequest, LeanWorkerOutputBudgets,
};

fn fixture_root() -> Option<PathBuf> {
    let root = std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok()?;
    Some(PathBuf::from(root))
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn open_call_shutdown_round_trip() {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let meta = LakeProjectMeta::from_explicit(&root).expect("meta");
    let project = LeanProject::open(meta, &default_cache_dir()).expect("open");

    // Trivial closure: just confirm we can dispatch work to the actor and
    // get a Send + 'static value back.
    let request = LeanWorkerDeclarationInspectionRequest {
        name: "Nat.add_zero".to_owned(),
        fields: LeanWorkerDeclarationInspectionFields::default(),
        budgets: LeanWorkerOutputBudgets::default(),
    };
    let call = project
        .inspect_declaration(vec!["Init".to_owned()], request)
        .await
        .expect("call");
    assert!(call.runtime.worker_generation >= 1);

    project.shutdown();

    // After shutdown, calls return WorkerUnavailable. Give the closed-channel
    // path a moment to propagate.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let request = LeanWorkerDeclarationInspectionRequest {
        name: "Nat.add_zero".to_owned(),
        fields: LeanWorkerDeclarationInspectionFields::default(),
        budgets: LeanWorkerOutputBudgets::default(),
    };
    let post = project.inspect_declaration(vec!["Init".to_owned()], request).await;
    assert!(
        matches!(post, Err(ServerError::WorkerUnavailable(_))),
        "call after shutdown must return WorkerUnavailable; got {post:?}"
    );
}
