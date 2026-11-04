//! Real-worker lifecycle checks for the process-local admission lease.

use std::time::Duration;

use lean_host_mcp::broker::{RuntimeAction, RuntimeControlRequest, RuntimeControlStatus};
use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint};

#[tokio::test]
#[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and its matching installed worker"]
async fn idle_lease_preserves_cache_across_reaper_ticks() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::path::PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE")?);
    let broker = ProjectBroker::new(BrokerConfig {
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: Duration::from_nanos(1),
    });
    let first = broker.admitted_project_runtime(ProjectHint::Default, vec![]).await?;
    let lease = broker.runtime_control(&RuntimeControlRequest {
        action: RuntimeAction::Acquire,
        ..RuntimeControlRequest::default()
    });
    assert_eq!(lease.status, RuntimeControlStatus::Acquired);
    broker.reap_idle();
    assert_eq!(broker.resident_paths().len(), 1);
    let token = lease.lease_id.ok_or("missing lease token")?;
    let released = broker.runtime_control(&RuntimeControlRequest {
        action: RuntimeAction::Release,
        lease_id: Some(token),
        ..RuntimeControlRequest::default()
    });
    assert_eq!(released.status, RuntimeControlStatus::Ok);
    let warm = broker.admitted_project_runtime(ProjectHint::Default, vec![]).await?;
    assert_eq!(first.freshness.session_id, warm.freshness.session_id);
    broker.reap_idle();
    assert!(broker.resident_paths().is_empty());
    Ok(())
}
