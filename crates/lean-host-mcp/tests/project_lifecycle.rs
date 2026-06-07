//! Lifecycle smoke test for the broker/project runtime: open against a real
//! fixture, run one trivial typed operation, and confirm the response exposes
//! runtime facts. Gated on `LEAN_HOST_MCP_TEST_FIXTURE` for the same reason
//! as `tests/worker.rs`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::significant_drop_tightening
)]

use std::path::PathBuf;

use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint};
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
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
        semantic_permits: BrokerConfig::default_semantic_permits(),
        semantic_waiters: BrokerConfig::default_semantic_waiters(),
        semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
        semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
    });

    // Trivial operation: just confirm we can dispatch typed work to the actor
    // and get runtime metadata back.
    let request = LeanWorkerDeclarationInspectionRequest {
        name: "Nat.add_zero".to_owned(),
        fields: LeanWorkerDeclarationInspectionFields::default(),
        budgets: LeanWorkerOutputBudgets::default(),
    };
    let call = broker
        .inspect_declaration(
            ProjectHint::Default,
            vec!["Init".to_owned()],
            vec!["Init".to_owned()],
            request,
        )
        .await
        .expect("call");
    assert!(call.runtime.worker_generation >= 1);
    assert!(!call.freshness.session_id.is_empty());
}
