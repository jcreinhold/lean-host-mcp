//! One admitted declaration on an existing prepared project actor.
use std::{
    os::unix::ffi::OsStrExt as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use lean_rs_worker_parent::{
    LeanWorkerCancellationToken, LeanWorkerDeclarationInspectionRequest, LeanWorkerHostHandle,
};
use lean_toolchain::native_guardian::{NativeGuardianDrainBinding, registry_directory};
use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::native_store::store::{
    AdmissionDeadline, Binding, CallerKey, Capability, Consume, Gate, NativeKey, NativeStore, Problem, Reject,
    ResourceReport, SavedRequest, Scope, recovery::RegistryIdentity,
};
use crate::{ProjectBroker, error::ServerError};

#[path = "trace_snapshot.rs"]
mod trace_snapshot;

#[path = "readiness_writer.rs"]
pub(crate) mod readiness_writer;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    job: Vec<u8>,
    revision: u64,
    prepared_sha256: [u8; 32],
    imports: Vec<String>,
    declaration: String,
}

pub(crate) struct Job {
    store: Arc<Mutex<NativeStore>>,
    original: SavedRequest,
    intent: Intent,
    deadline: AdmissionDeadline,
}

struct StopRegistration<'a> {
    store: &'a Mutex<NativeStore>,
    binding: Binding,
}

impl Drop for StopRegistration<'_> {
    fn drop(&mut self) {
        self.store.lock().detach_active_stop(self.binding);
    }
}

fn internal(error: impl std::fmt::Display) -> ServerError {
    ServerError::Internal(error.to_string())
}

impl Job {
    pub(crate) fn imports(&self) -> &[String] {
        &self.intent.imports
    }

    pub(crate) fn run(
        self,
        worker: &mut LeanWorkerHostHandle,
        operator_prepared: Option<&str>,
        actor_healthy: &AtomicBool,
    ) -> crate::error::Result<()> {
        if operator_prepared
            .is_none_or(|input| Sha256::digest(input.as_bytes()).as_slice() != self.intent.prepared_sha256)
        {
            return Err(internal("prepared generation changed or original admission expired"));
        }

        self.deadline.remaining().map_err(internal)?;
        {
            let mut store = self.store.lock();
            let current = store
                .client_get(self.original.key, &self.original.capability)
                .map_err(internal)?;
            drop(store);
            if current.gate != Gate::Pending || current.scope.is_some() {
                return Ok(());
            }
        }
        // Loan preparation performs only bounded storage work on this exact
        // receiver. If admission fails later, this actor still owns retirement.
        let original = worker.managed_guardian_binding().map_err(internal)?;
        actor_healthy.store(false, Ordering::Release);
        let expected = worker
            .prepare_managed_artifact_read(self.intent.imports.clone(), self.deadline.expires_ns)
            .map_err(internal)?;
        if expected != original {
            return Err(internal("prepared receiver changed or original deadline elapsed"));
        }
        let registry = RegistryIdentity {
            directory: registry_directory().map_err(internal)?.as_os_str().as_bytes().to_vec(),
            device: expected.registry_device,
            inode: expected.registry_inode,
            generation: expected.generation.clone(),
            boot: uuid::Uuid::parse_str(&expected.boot).map_err(internal)?.into_bytes(),
            parent: expected.parent.clone(),
            worker: expected.worker.clone(),
            guardian: expected.guardian.clone(),
        };
        let scope = Scope {
            id: uuid::Uuid::new_v4().into_bytes(),
            generation: 1,
            owner: uuid::Uuid::new_v4().into_bytes(),
        };
        let cancellation = LeanWorkerCancellationToken::new();
        let (binding, permit) = {
            let mut store = self.store.lock();
            let current = store
                .client_get(self.original.key, &self.original.capability)
                .map_err(internal)?;
            if current.gate != Gate::Pending || current.scope.is_some() {
                return Ok(());
            }
            let binding = store
                .bind_timed_recovery_scope(&self.original, scope, &registry, self.deadline.expires_ns)
                .map_err(internal)?;
            store
                .observe_resource(binding, &self.original.capability, ResourceReport::Open)
                .map_err(internal)?;
            let Consume::Granted(permit) = store
                .consume_launch(binding, &self.original.capability)
                .map_err(internal)?
            else {
                return Ok(());
            };
            let permit = permit
                .into_deadline_grant()
                .ok_or_else(|| internal("timed grant missing"))?;
            store
                .attach_active_stop(binding, cancellation.clone())
                .map_err(internal)?;
            drop(store);
            (binding, permit)
        };
        let _stop = StopRegistration {
            store: &self.store,
            binding,
        };

        // The ordinary actor lifetime owns the capacity permit until closure.
        let (admitted, admitted_registry, expires_ns, intent) = permit.into_parts();
        if admitted != binding
            || admitted_registry != registry
            || expires_ns != self.deadline.expires_ns
            || intent != self.original.intent
        {
            return Err(internal("consumed grant differs from the original stage"));
        }
        if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
            && std::env::var_os("LEAN_HOST_MCP_TEST_STOP_BEFORE_ARM").is_some()
        {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)
                .map_err(internal)?;
        }
        let outcome = worker
            .inspect_admitted_declaration_with_imports_once_cancellable(
                &self.intent.imports,
                LeanWorkerDeclarationInspectionRequest::new(self.intent.declaration),
                &expected,
                admitted.key.nonce,
                admitted.key.stage,
                expires_ns,
                Some(&cancellation),
            )
            .map_err(internal)?;
        let mut store = self.store.lock();
        let result_saved = match &outcome.result {
            Ok(result) => serde_json::to_vec(result).map_err(internal).and_then(|body| {
                store
                    .observe_result(binding, &self.original.capability, binding.key.nonce, &body)
                    .map_err(internal)
            }),
            Err(error) => {
                tracing::debug!(%error, "managed declaration result unavailable");
                Ok(())
            }
        };
        let closure_saved = if outcome.retirement.is_ok() {
            store
                .observe_resource(binding, &self.original.capability, ResourceReport::Closed)
                .map_err(internal)
        } else {
            Ok(())
        };
        result_saved.and(closure_saved)
    }
}

fn public_error(error: &Problem) -> McpError {
    let reason = match error {
        Problem::Rejected(Reject::CapabilityDenied) => "capability_denied",
        Problem::Rejected(Reject::UnknownStage) => "unknown_stage",
        Problem::Rejected(Reject::WrongStore) => "wrong_store_or_epoch",
        Problem::Rejected(_) | Problem::Sql(_) | Problem::Connection(_) => "store_unavailable",
    };
    McpError::invalid_params(reason, None)
}

pub(crate) fn advertise(mut reply: serde_json::Value, broker: &ProjectBroker) -> serde_json::Value {
    if let Some(fields) = reply.as_object_mut() {
        fields.insert("dispatch_enabled".into(), broker.prepared_generation().is_some().into());
        if fields.contains_key("acceptance_epoch") && trace_snapshot::available(broker) {
            let operations = vec![trace_snapshot::OPERATION];
            let operations = {
                let mut operations = operations;
                if readiness_writer::selection().is_ok() {
                    operations.extend([
                        readiness_writer::OPERATION,
                        crate::writer_preparation::OPERATION,
                        crate::writer_preparation::VERIFIER_OPERATION,
                        readiness_writer::CONSUMER,
                        readiness_writer::SETUP_VERIFICATION,
                        readiness_writer::SETUP_DIAGNOSTICS,
                        readiness_writer::SETUP_OUTLINE,
                        readiness_writer::SETUP_TRIAL,
                        readiness_writer::TOOLING_SUITES,
                        readiness_writer::REGROUP,
                        readiness_writer::WHOLE_LINT,
                        readiness_writer::FIRST_PARTY_CLEAN,
                    ]);
                    if let Ok(root) = broker.native_root(&crate::broker::ProjectHint::Default)
                        && let Some((digest, preview)) = readiness_writer::formatter_approval(&root)
                    {
                        operations.push(readiness_writer::FORMAT_FIX);
                        if preview {
                            operations.push(readiness_writer::FORMAT_PREVIEW);
                        }
                        fields.insert("formatter_sha256".into(), serde_json::json!(digest));
                    }
                }
                operations
            };
            fields.insert("operations".into(), serde_json::json!(operations));
        }
    }
    reply
}

pub(crate) async fn start(
    broker: &ProjectBroker,
    store: &Arc<Mutex<NativeStore>>,
    key: NativeKey,
    capability: Capability,
) -> Result<serde_json::Value, McpError> {
    if capability.0 == [0; 32] {
        return Err(McpError::invalid_params("capability_denied", None));
    }
    let get = || {
        let payload = serde_json::json!({"version":1,"command":{"op":"get","key":key,"capability":capability.0}});
        let mut store = store.lock();
        let _recovery = reconcile_lookup(&mut store, key, &capability);
        store
            .admission(payload.to_string().as_bytes())
            .map(|reply| advertise(reply, broker))
            .map_err(|reason| McpError::invalid_params(reason, None))
    };
    let snapshot = store
        .lock()
        .client_get(key, &capability)
        .map_err(|error| public_error(&error))?;
    if snapshot.gate != Gate::Pending || snapshot.scope.is_some() {
        return get();
    }
    if readiness_writer::is_operation(&snapshot.intent) {
        readiness_writer::run(broker, store, snapshot, capability.clone()).await?;
        return get();
    }
    if trace_snapshot::is_operation(&snapshot.intent) {
        trace_snapshot::run(broker, store, snapshot, capability.clone()).await?;
        return get();
    }
    if broker.prepared_generation().is_none() {
        return Err(McpError::invalid_params("operation_disabled", None));
    }
    let intent: Intent = serde_json::from_slice(&snapshot.intent)
        .map_err(|_| McpError::invalid_params("invalid_inspection_intent", None))?;
    if intent.job.is_empty() || intent.imports.is_empty() || intent.declaration.is_empty() {
        return Err(McpError::invalid_params("invalid_inspection_intent", None));
    }
    let configured = broker
        .prepared_generation()
        .ok_or_else(|| McpError::invalid_params("no_prepared_project", None))?;
    if Sha256::digest(configured.as_bytes()).as_slice() != intent.prepared_sha256 {
        return Err(McpError::invalid_params("prepared_generation_changed", None));
    }
    let admitted = snapshot
        .admission_deadline
        .ok_or_else(|| McpError::invalid_params("missing_admission_deadline", None))?;
    let now = Instant::now();
    let remaining = admitted
        .remaining()
        .map_err(|_| McpError::invalid_params("admission_deadline_unavailable_or_expired", None))?;
    let deadline = now
        .checked_add(remaining)
        .ok_or_else(|| McpError::internal_error("deadline_overflow", None))?;
    let actor = broker
        .managed_actor(deadline.into())
        .await
        .map_err(|error| {
            tracing::debug!(%error, "managed inspection could not acquire its prepared actor");
            McpError::internal_error("prepared_actor_unavailable", None)
        })?;
    let original = SavedRequest {
        caller: CallerKey {
            job: intent.job.clone(),
            stage: key.stage,
            revision: intent.revision,
        },
        key,
        intent: snapshot.intent,
        capability: capability.clone(),
    };
    let job = Job {
        store: Arc::clone(store),
        original,
        intent,
        deadline: admitted,
    };
    if let Err(error) = actor.inspect_managed(job, deadline).await {
        tracing::debug!(%error, "managed inspection did not confirm a result");
        let saved = store
            .lock()
            .client_get(key, &capability)
            .map_err(|error| public_error(&error))?;
        if saved.gate == Gate::Pending && saved.scope.is_none() {
            return Err(McpError::internal_error("prepared_inspection_not_started", None));
        }
    }
    get()
}

/// A replacement reconciles only the stage selected by this authenticated lookup.
pub(crate) fn reconcile_lookup(
    store: &mut NativeStore,
    key: NativeKey,
    capability: &Capability,
) -> Option<serde_json::Value> {
    if capability.0 == [0; 32] {
        return None;
    }
    if let Err(error) = store.reconcile_trace_read(key, capability) {
        tracing::debug!(%error, "original trace reader closure remains unresolved");
    }
    let Ok(snapshot) = store.client_get(key, capability) else {
        return None;
    };
    if snapshot.scope.is_none() || snapshot.resource_report == ResourceReport::Closed {
        return None;
    }
    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    let external = crate::native_reader::admitted::recovery_request(key, snapshot.intent.clone(), capability.clone());
    #[cfg(not(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux"))))]
    let external: Option<SavedRequest> = None;
    let external_reader = external.is_some();
    let saved = if let Some(saved) = external {
        saved
    } else {
        let Ok(intent) = serde_json::from_slice::<Intent>(&snapshot.intent) else {
            return None;
        };
        SavedRequest {
            caller: CallerKey {
                job: intent.job,
                stage: key.stage,
                revision: intent.revision,
            },
            key,
            intent: snapshot.intent,
            capability: capability.clone(),
        }
    };
    let Ok((stage, identity)) = store.open_stage_recovery(&saved, None) else {
        return None;
    };
    match observe_recovered_closure(&identity).and_then(|closed| {
        if closed {
            store
                .reconcile_closed_scope(stage, &identity)
                .map_err(std::io::Error::other)?;
        }
        Ok(closed)
    }) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(error) => tracing::debug!(%error, "original managed scope closure remains unresolved"),
    }
    if cfg!(target_os = "macos") && !external_reader {
        return Some(serde_json::json!({
            "generation":identity.generation,"revision":saved.caller.revision,
        }));
    }
    None
}

fn observe_recovered_closure(identity: &RegistryIdentity) -> std::io::Result<bool> {
    use lean_toolchain::native_guardian::{NativeOwnerObservation, observe_original_guardian_closure};
    use std::{fs, io};

    let registry = std::path::Path::new(std::ffi::OsStr::from_bytes(&identity.directory));
    if registry != registry_directory()? || !identity.matches_directory() {
        return Ok(false);
    }
    match observe_original_guardian_closure(registry, &native_binding(identity), Duration::from_millis(100)) {
        Ok(closed) => Ok(closed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let generation = registry.join(format!("worker-{}", identity.generation));
            if !matches!(fs::symlink_metadata(generation), Err(error) if error.kind() == io::ErrorKind::NotFound)
                || !identity.matches_directory()
            {
                return Ok(false);
            }
            // The authenticated record can disappear after independent group
            // absence. A missing record itself supplies no closure evidence.
            Ok(identity.worker.observe()? == NativeOwnerObservation::Absent
                && !identity.guardian.is_live()?
                && identity.matches_directory())
        }
        Err(error) => Err(error),
    }
}

fn native_binding(identity: &RegistryIdentity) -> NativeGuardianDrainBinding {
    NativeGuardianDrainBinding {
        registry_device: identity.device,
        registry_inode: identity.inode,
        generation: identity.generation.clone(),
        boot: uuid::Uuid::from_bytes(identity.boot).to_string(),
        parent: identity.parent.clone(),
        worker: identity.worker.clone(),
        guardian: identity.guardian.clone(),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn drain(
    store: &mut NativeStore,
    claim: crate::native_store::store::boundary::DrainClaim,
) -> Result<serde_json::Value, McpError> {
    use lean_toolchain::native_guardian::{
        NativeGuardianDrainStatus, observe_original_guardian_drain, submit_original_guardian_drain,
    };
    let crate::native_store::store::boundary::DrainClaim {
        key,
        capability,
        generation,
        revision,
        submit,
        authorization,
        claimant,
    } = claim;
    if capability.0 == [0; 32] {
        return Err(McpError::invalid_params("capability_denied", None));
    }
    let snapshot = store
        .client_get(key, &capability)
        .map_err(|error| public_error(&error))?;
    if submit {
        store
            .authorize_drain(key, authorization)
            .map_err(|error| public_error(&error))?;
    }
    let intent: Intent = serde_json::from_slice(&snapshot.intent)
        .map_err(|_| McpError::invalid_params("recovery_claim_changed", None))?;
    if intent.revision != revision {
        return Err(McpError::invalid_params("recovery_claim_changed", None));
    }
    let saved = SavedRequest {
        caller: CallerKey {
            job: intent.job,
            stage: key.stage,
            revision,
        },
        key,
        intent: snapshot.intent,
        capability,
    };
    let (binding, identity) = store
        .open_stage_recovery(&saved, Some(&generation))
        .map_err(|_| McpError::invalid_params("recovery_claim_changed", None))?;
    let registry = std::path::Path::new(std::ffi::OsStr::from_bytes(&identity.directory));
    if registry_directory().ok().as_deref() != Some(registry) || !identity.matches_directory() {
        return Err(McpError::invalid_params("recovery_claim_changed", None));
    }
    let expected = native_binding(&identity);
    let unavailable = |_| McpError::internal_error("drain_unavailable", None);
    let mut claim_status = None;
    let disposition = if submit {
        let won = store
            .claim_recovery(&saved, binding, &identity, authorization, claimant)
            .map_err(|error| match error {
                Problem::Rejected(Reject::RecoveryClaimBusy) => McpError::invalid_params("recovery_claim_busy", None),
                Problem::Rejected(Reject::CallerBindingChanged | Reject::WrongScope | Reject::IntentChanged) => {
                    McpError::invalid_params("recovery_claim_changed", None)
                }
                Problem::Rejected(_) | Problem::Sql(_) | Problem::Connection(_) => public_error(&error),
            })?;
        claim_status = Some(if won { "won" } else { "same" });
        if won
            && option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
            && std::env::var_os("LEAN_HOST_MCP_TEST_STOP_AFTER_CLAIM").is_some()
        {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)
                .map_err(std::io::Error::from)
                .map_err(unavailable)?;
        }
        // The durable claim survives any failed delivery or lost response.
        submit_original_guardian_drain(registry, &expected).map_err(unavailable)?;
        "offered"
    } else {
        match observe_original_guardian_drain(registry, &expected).map_err(unavailable)? {
            NativeGuardianDrainStatus::Empty => "empty",
            NativeGuardianDrainStatus::Pending => "pending",
            NativeGuardianDrainStatus::Refused => "refused",
            NativeGuardianDrainStatus::Accepted => "accepted",
        }
    };
    Ok(serde_json::json!({"drain":disposition,"key":key,"claim":claim_status}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{broker::BrokerConfig, project::ProjectRuntimeConfig};
    use lean_toolchain::native_guardian::NativeProcessIdentity;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    #[tokio::test]
    async fn original_admission_deadline_survives_retry_restart_and_binding_contention() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("stages.sqlite3");
        let prepared = serde_json::json!({"plan":{"project":root}}).to_string();
        let runtime = ProjectRuntimeConfig::default()
            .with_operator_primary(Some(&root))
            .unwrap()
            .with_prepared_generation(Some(prepared.clone()))
            .unwrap();
        let broker = ProjectBroker::new_with_runtime_config(
            BrokerConfig {
                env_default: Some(root.clone()),
                config_default: None,
                cwd: root.clone(),
                max_projects: std::num::NonZeroUsize::MIN,
                max_workers_per_project: std::num::NonZeroUsize::MIN,
                idle_timeout: Duration::ZERO,
            },
            runtime,
        );
        let store = Arc::new(Mutex::new(NativeStore::create_new(&path).unwrap()));
        let capability = Capability([1; 32]);
        let intent = serde_json::to_vec(&serde_json::json!({
            "job":[1], "revision":1,
            "prepared_sha256":Sha256::digest(prepared.as_bytes()).to_vec(),
            "imports":["Init"], "declaration":"Nat.add_zero",
        }))
        .unwrap();
        let key = NativeKey {
            store: store.lock().identity(),
            epoch: 1,
            nonce: [1; 16],
            stage: 1,
        };
        let create = |key, budget| {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "version":1,"command":{"op":"create","key":key,"capability":capability.0,"intent":intent}
            }))
            .unwrap();
            store.lock().admission_with_budget(&bytes, budget).unwrap()
        };
        let deadline = |key| store.lock().client_get(key, &capability).unwrap().admission_deadline;

        create(key, Some(Duration::from_secs(60)));
        let original = deadline(key).unwrap();
        for _ in 0..2 {
            assert_eq!(
                start(&broker, &store, key, capability.clone())
                    .await
                    .unwrap_err()
                    .message,
                "prepared_actor_unavailable"
            );
            create(key, Some(Duration::from_secs(180)));
            assert_eq!(deadline(key), Some(original));
        }
        *store.lock() = NativeStore::open(&path).unwrap();
        assert_eq!(
            start(&broker, &store, key, capability.clone())
                .await
                .unwrap_err()
                .message,
            "prepared_actor_unavailable"
        );
        assert_eq!(deadline(key), Some(original));

        let saved = SavedRequest {
            caller: CallerKey {
                job: vec![1],
                stage: 1,
                revision: 1,
            },
            key,
            intent: intent.clone(),
            capability: capability.clone(),
        };
        let meta = root.metadata().unwrap();
        let process = |pid| NativeProcessIdentity {
            version: 1,
            pid,
            start: format!("test-{pid}"),
            session: 1,
            process_group: 2,
        };
        let identity = RegistryIdentity {
            directory: root.as_os_str().as_bytes().to_vec(),
            device: meta.dev(),
            inode: meta.ino(),
            generation: "a".repeat(48),
            boot: original.boot,
            parent: process(10),
            worker: process(11),
            guardian: process(12),
        };
        let scope = Scope {
            id: [4; 16],
            generation: 1,
            owner: [5; 16],
        };
        let holder = rusqlite::Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(
            store.lock().bind_timed_recovery_scope(&saved, scope, &identity, original.expires_ns),
            Err(Problem::Sql(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ));
        holder.execute_batch("ROLLBACK").unwrap();
        assert_eq!(store.lock().client_get(key, &capability).unwrap().scope, None);
        assert_eq!(deadline(key), Some(original));
        assert!(matches!(
            store
                .lock()
                .bind_timed_recovery_scope(&saved, scope, &identity, original.expires_ns + 1),
            Err(Problem::Rejected(Reject::InvalidLaunchDeadline))
        ));
        holder
            .execute_batch(
                "CREATE TRIGGER reject_binding BEFORE INSERT ON recovery_bindings
             BEGIN SELECT RAISE(ABORT, 'binding failed'); END;",
            )
            .unwrap();
        assert!(matches!(
            store
                .lock()
                .bind_timed_recovery_scope(&saved, scope, &identity, original.expires_ns),
            Err(Problem::Sql(_))
        ));
        holder.execute_batch("DROP TRIGGER reject_binding").unwrap();
        assert_eq!(store.lock().client_get(key, &capability).unwrap().scope, None);
        assert_eq!(deadline(key), Some(original));

        let bound = store
            .lock()
            .bind_timed_recovery_scope(&saved, scope, &identity, original.expires_ns)
            .unwrap();
        let Consume::Granted(grant) = store.lock().consume_launch(bound, &capability).unwrap() else {
            panic!("the original deadline must authorize one grant");
        };
        assert_eq!(grant.into_deadline_grant().unwrap().into_parts().2, original.expires_ns);
        *store.lock() = NativeStore::open(&path).unwrap();
        let consumed = start(&broker, &store, key, capability.clone()).await.unwrap();
        assert_eq!(consumed["execution"], "unknown");
        assert_eq!(deadline(key), Some(original));
        assert!(matches!(
            store.lock().consume_launch(bound, &capability).unwrap(),
            Consume::AlreadyConsumed
        ));

        let legacy = NativeKey { nonce: [2; 16], ..key };
        create(legacy, None);
        create(legacy, Some(Duration::from_secs(60)));
        assert_eq!(deadline(legacy), None);
        assert_eq!(
            start(&broker, &store, legacy, capability.clone())
                .await
                .unwrap_err()
                .message,
            "missing_admission_deadline"
        );

        let expired = NativeKey { nonce: [3; 16], ..key };
        create(expired, Some(Duration::from_millis(1)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let wrong_boot = NativeKey { nonce: [4; 16], ..key };
        create(wrong_boot, Some(Duration::from_secs(60)));
        let mut data = holder
            .query_row(
                "SELECT admission_deadline FROM stages WHERE nonce=?1",
                [wrong_boot.nonce.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        data[0] ^= 1;
        holder
            .execute(
                "UPDATE stages SET admission_deadline=?1 WHERE nonce=?2",
                rusqlite::params![data, wrong_boot.nonce],
            )
            .unwrap();
        for refused in [expired, wrong_boot] {
            let prior = deadline(refused);
            create(refused, Some(Duration::from_secs(180)));
            assert_eq!(
                start(&broker, &store, refused, capability.clone())
                    .await
                    .unwrap_err()
                    .message,
                "admission_deadline_unavailable_or_expired"
            );
            assert_eq!(deadline(refused), prior);
        }
        holder
            .execute_batch("ALTER TABLE stages DROP COLUMN admission_deadline")
            .unwrap();
        assert_eq!(
            NativeStore::read_recovery_scope(&path, &saved, None, None).unwrap(),
            (bound, identity)
        );
        *store.lock() = NativeStore::open(&path).unwrap();
        assert!(matches!(
            store.lock().consume_launch(bound, &capability).unwrap(),
            Consume::AlreadyConsumed
        ));
    }
}
