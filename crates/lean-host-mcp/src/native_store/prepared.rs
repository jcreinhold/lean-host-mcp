//! One admitted declaration on an existing prepared project actor.
use std::{
    os::unix::ffi::OsStrExt as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use lean_rs_worker_parent::{LeanWorkerDeclarationInspectionRequest, LeanWorkerHostHandle};
use lean_toolchain::native_guardian::{NativeGuardianDrainBinding, guardian_deadline_ticks, registry_directory};
use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::native_store::store::{
    CallerKey, Capability, Consume, Gate, NativeKey, NativeStore, Problem, Reject, ResourceReport, SavedRequest, Scope,
    recovery::RegistryIdentity,
};
use crate::{ProjectBroker, error::ServerError};

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
    expiry: u64,
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
        if !operator_prepared
            .is_some_and(|input| Sha256::digest(input.as_bytes()).as_slice() == self.intent.prepared_sha256)
            || guardian_deadline_ticks().map_err(internal)?.as_nanos() >= u128::from(self.expiry)
        {
            return Err(internal("prepared generation changed or original admission expired"));
        }

        {
            let mut store = self.store.lock();
            let current = store
                .client_get(self.original.key, &self.original.capability)
                .map_err(internal)?;
            if current.gate != Gate::Pending || current.scope.is_some() {
                return Ok(());
            }
        }
        // Loan preparation performs only bounded storage work on this exact
        // receiver. If admission fails later, this actor still owns retirement.
        let original = worker.managed_guardian_binding().map_err(internal)?;
        actor_healthy.store(false, Ordering::Release);
        let expected = worker
            .prepare_managed_artifact_read(self.intent.imports.clone(), self.expiry)
            .map_err(internal)?;
        if expected != original || guardian_deadline_ticks().map_err(internal)?.as_nanos() >= u128::from(self.expiry) {
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
        let (binding, permit) = {
            let mut store = self.store.lock();
            let current = store
                .client_get(self.original.key, &self.original.capability)
                .map_err(internal)?;
            if current.gate != Gate::Pending || current.scope.is_some() {
                return Ok(());
            }
            let binding = store
                .bind_timed_recovery_scope(&self.original, scope, &registry, self.expiry)
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
            (
                binding,
                permit
                    .into_deadline_grant()
                    .ok_or_else(|| internal("timed grant missing"))?,
            )
        };

        // The ordinary actor lifetime owns the capacity permit until closure.
        let (admitted, admitted_registry, expires_ns, intent) = permit.into_parts();
        if admitted != binding
            || admitted_registry != registry
            || expires_ns != self.expiry
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
            .inspect_admitted_declaration_with_imports_once(
                &self.intent.imports,
                LeanWorkerDeclarationInspectionRequest::new(self.intent.declaration),
                &expected,
                admitted.key.nonce,
                admitted.key.stage,
                expires_ns,
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

fn public_error(error: Problem) -> McpError {
    let reason = match error {
        Problem::Rejected(Reject::CapabilityDenied) => "capability_denied",
        Problem::Rejected(Reject::UnknownStage) => "unknown_stage",
        Problem::Rejected(Reject::WrongStore) => "wrong_store_or_epoch",
        _ => "store_unavailable",
    };
    McpError::invalid_params(reason, None)
}

pub(crate) fn advertise(mut reply: serde_json::Value, broker: &ProjectBroker) -> serde_json::Value {
    reply["dispatch_enabled"] = serde_json::Value::Bool(broker.prepared_generation().is_some());
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
    let snapshot = store.lock().client_get(key, &capability).map_err(public_error)?;
    if snapshot.gate != Gate::Pending || snapshot.scope.is_some() {
        return get();
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
    let budget = Duration::from_millis(broker.request_timeout_millis());
    let now = Instant::now();
    let expiry = guardian_deadline_ticks()
        .ok()
        .and_then(|now| now.checked_add(budget))
        .and_then(|end| u64::try_from(end.as_nanos()).ok())
        .filter(|end| i64::try_from(*end).is_ok())
        .ok_or_else(|| McpError::internal_error("boot_deadline_unavailable", None))?;
    let deadline = now
        .checked_add(budget)
        .ok_or_else(|| McpError::internal_error("deadline_overflow", None))?;
    let actor = broker
        .managed_actor(deadline.into())
        .await
        .map_err(|_| McpError::internal_error("prepared_actor_unavailable", None))?;
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
        expiry,
    };
    if let Err(error) = actor.inspect_managed(job, deadline).await {
        tracing::debug!(%error, "managed inspection did not confirm a result");
        let saved = store.lock().client_get(key, &capability).map_err(public_error)?;
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
    let Ok(snapshot) = store.client_get(key, capability) else {
        return None;
    };
    if snapshot.scope.is_none() || snapshot.resource_report == ResourceReport::Closed {
        return None;
    }
    let Ok(intent) = serde_json::from_slice::<Intent>(&snapshot.intent) else {
        return None;
    };
    let saved = SavedRequest {
        caller: CallerKey {
            job: intent.job,
            stage: key.stage,
            revision: intent.revision,
        },
        key,
        intent: snapshot.intent,
        capability: capability.clone(),
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
    if cfg!(target_os = "macos") {
        return Some(serde_json::json!({
            "generation":identity.generation,"revision":intent.revision,
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
    } = claim;
    if capability.0 == [0; 32] {
        return Err(McpError::invalid_params("capability_denied", None));
    }
    let snapshot = store.client_get(key, &capability).map_err(public_error)?;
    if submit {
        store.authorize_drain(key, authorization).map_err(public_error)?;
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
    let (_, identity) = store
        .open_stage_recovery(&saved, Some(&generation))
        .map_err(|_| McpError::invalid_params("recovery_claim_changed", None))?;
    let registry = std::path::Path::new(std::ffi::OsStr::from_bytes(&identity.directory));
    if registry_directory().ok().as_deref() != Some(registry) || !identity.matches_directory() {
        return Err(McpError::invalid_params("recovery_claim_changed", None));
    }
    let expected = native_binding(&identity);
    let unavailable = |_| McpError::internal_error("drain_unavailable", None);
    let disposition = if submit {
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
    Ok(serde_json::json!({"drain":disposition,"key":key}))
}
