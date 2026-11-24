//! Fixed Lake provenance read with an original-key managed result.
use std::{io, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use serde::Deserialize;

use crate::{
    ProjectBroker,
    artifact_reads::{ArtifactReadGuard, lake_trace_paths},
    broker::ProjectHint,
    native_store::store::{AdmissionDeadline, Capability, NativeStore, ResourceReport, Snapshot},
};

pub(super) const OPERATION: &str = "lake_artifact_traces_v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    operation: String,
    job: Vec<u8>,
    revision: u64,
    project: PathBuf,
    targets: Vec<String>,
}

pub(super) fn available(broker: &ProjectBroker) -> bool {
    broker.native_root(&ProjectHint::Default).is_ok()
}

pub(super) fn is_operation(intent: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(intent)
        .is_ok_and(|value| value.get("operation").and_then(serde_json::Value::as_str) == Some(OPERATION))
}

pub(super) async fn run(
    broker: &ProjectBroker,
    store: &Arc<Mutex<NativeStore>>,
    original: Snapshot,
    capability: Capability,
) -> Result<(), McpError> {
    let invalid = |reason| McpError::invalid_params(reason, None);
    let intent: Intent = serde_json::from_slice(&original.intent).map_err(|_| invalid("invalid_trace_intent"))?;
    if intent.operation != OPERATION
        || intent.job.is_empty()
        || intent.revision == 0
        || lake_trace_paths(&intent.targets).is_none()
    {
        return Err(invalid("invalid_trace_intent"));
    }
    let root = broker
        .native_root(&ProjectHint::Default)
        .map_err(|_| invalid("trace_project_unavailable"))?;
    if root.as_os_str() != intent.project.as_os_str() {
        return Err(invalid("trace_project_changed"));
    }
    let deadline = original
        .admission_deadline
        .ok_or_else(|| invalid("missing_admission_deadline"))?;
    let slot = broker
        .try_managed_stage_slot(&root)
        .map_err(|_| McpError::internal_error("trace_reader_busy", None))?;
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        let binding = store
            .lock()
            .consume_trace_read(original.key, &capability, &original.intent)
            .map_err(|error| super::public_error(&error))?;
        let Some(binding) = binding else { return Ok(()) };
        let body = match snapshot(&root, &intent.targets, deadline) {
            Ok(traces) => serde_json::json!({"version": 1, "traces": traces}),
            Err(error) => {
                let code = if error.kind() == io::ErrorKind::InvalidData {
                    "artifact_trace_alias"
                } else if error.kind() == io::ErrorKind::TimedOut && deadline.remaining().is_err() {
                    "admission_deadline_unavailable_or_expired"
                } else {
                    "artifact_read_unavailable"
                };
                serde_json::json!({"version": 1, "error": code})
            }
        };
        // The reader has closed all its descriptors before publishing closure.
        let mut store = store.lock();
        store
            .observe_resource(binding, &capability, ResourceReport::Closed)
            .map_err(|error| super::public_error(&error))?;
        store
            .observe_result(
                binding,
                &capability,
                uuid::Uuid::new_v4().into_bytes(),
                body.to_string().as_bytes(),
            )
            .map_err(|error| super::public_error(&error))
    })
    .await
    .map_err(|_| McpError::internal_error("trace_reader_unavailable", None))?
}

fn snapshot(
    root: &std::path::Path,
    targets: &[String],
    deadline: AdmissionDeadline,
) -> io::Result<Option<std::collections::BTreeMap<String, String>>> {
    let remaining = || {
        deadline
            .remaining()
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))
    };
    let budget = remaining()?.min(Duration::from_millis(250));
    if std::fs::symlink_metadata(root.join(".lake")).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let guard = ArtifactReadGuard::acquire(root, budget)?;
    guard.lake_trace_snapshot(root, targets, || remaining().map(|_| ()))
}

#[cfg(test)]
mod tests {
    use super::super::{advertise, start};
    use super::*;
    use crate::{broker::BrokerConfig, native_store::store::NativeKey, project::ProjectRuntimeConfig};
    use serde_json::json;
    use std::num::NonZeroUsize;

    #[tokio::test]
    async fn managed_trace_shares_capacity_and_reopens_without_rereading_artifacts() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().canonicalize().unwrap();
        let root = directory.join("project");
        let build = root.join(".lake/build/lib/lean");
        std::fs::create_dir_all(&build).unwrap();
        let trace = build.join("Demo.trace");
        std::fs::write(root.join("lakefile.toml"), "name = 'demo'\n").unwrap();
        std::fs::write(
            &trace,
            br#"{"synthetic":false,"inputs":{"files":1},"outputs":{"olean":2}}"#,
        )
        .unwrap();
        let runtime = ProjectRuntimeConfig::default()
            .with_operator_primary(Some(&root))
            .unwrap();
        let broker = ProjectBroker::new_with_runtime_config(
            BrokerConfig {
                env_default: Some(root.clone()),
                config_default: None,
                cwd: root.clone(),
                max_projects: NonZeroUsize::new(3).unwrap(),
                max_workers_per_project: NonZeroUsize::MIN,
                idle_timeout: Duration::ZERO,
            },
            runtime,
        );
        let database = directory.join("stages.sqlite3");
        let store = Arc::new(Mutex::new(NativeStore::create_new(&database).unwrap()));
        let capability = Capability([19; 32]);
        let key = NativeKey {
            store: store.lock().identity(),
            epoch: 1,
            nonce: [17; 16],
            stage: 1,
        };
        let command = |command| json!({"version":1,"command":command}).to_string();
        let discovery = store
            .lock()
            .admission(command(json!({"op":"discover"})).as_bytes())
            .unwrap();
        assert_eq!(advertise(discovery, &broker)["operations"], json!([OPERATION]));
        let intent = json!({"operation":OPERATION,"job":[1],"revision":1,"project":root,"targets":["Demo"]})
            .to_string()
            .into_bytes();
        let create = command(json!({"op":"create","key":key,"capability":capability.0,"intent":intent}));
        store
            .lock()
            .admission_with_budget(create.as_bytes(), Some(Duration::from_secs(10)))
            .unwrap();
        // Keep one warm actor permit and an independent readiness turn
        // charged while exercising the actual trace admission path.
        let warm = crate::worker_retirement::WorkerSlot::new(
            broker.try_managed_stage_slot(&directory.join("warm")).unwrap(),
            lean_rs_worker_parent::LeanWorkerProcessTracker::new(),
        );
        let readiness = crate::worker_retirement::ManagedWriterSlot::new(
            broker.try_managed_stage_slot(&directory.join("readiness")).unwrap(),
        );
        let occupied = broker.try_managed_stage_slot(&directory.join("other")).unwrap();
        assert_eq!(
            start(&broker, &store, key, capability.clone())
                .await
                .unwrap_err()
                .message,
            "trace_reader_busy"
        );
        drop(occupied);
        let result = start(&broker, &store, key, capability.clone()).await.unwrap();
        let status = broker.runtime_control(&crate::broker::RuntimeControlRequest::default());
        assert_eq!(status.pool.occupied_slots, 2);
        drop(readiness);
        drop(warm);
        assert!(
            broker
                .runtime_control(&crate::broker::RuntimeControlRequest::default())
                .pool
                .workers_stopped
        );
        assert_eq!(result["custody"], "closed");
        assert_eq!(result["result"]["state"], "retained");
        drop(store);

        // Even another authenticated START must return the original result.
        // A client that lost the reply uses GET, which also has no scan path.
        std::fs::write(trace, b"not a trace anymore").unwrap();
        let store = Arc::new(Mutex::new(NativeStore::open(&database).unwrap()));
        let get = command(json!({"op":"get","key":key,"capability":capability.0}));
        let reopened = store.lock().admission(get.as_bytes()).unwrap();
        assert_eq!(reopened, result);
        let repeated = start(&broker, &store, key, capability.clone()).await.unwrap();
        assert_eq!(repeated, result);
        let wrong = Capability([20; 32]);
        assert_eq!(
            start(&broker, &store, key, wrong).await.unwrap_err().message,
            "capability_denied"
        );
    }
}
