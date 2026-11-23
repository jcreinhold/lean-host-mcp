//! Matched development adapter for the SDK's exact warm finish callbacks.

use lean_rs_worker_parent::{LeanWorkerError, ManagedWarmInspectionOutcome};
use lean_rs_worker_protocol::protocol::ManagedInspectionCommand;

use super::*;
use crate::native_store::store::recovery::CheckedWarmFinish;

pub(crate) struct WarmGeneration {
    expected: NativeGuardianDrainBinding,
    imports: Vec<String>,
    command: u64,
}

fn callback_error(error: impl std::fmt::Display) -> LeanWorkerError {
    LeanWorkerError::Setup {
        message: error.to_string(),
    }
}

impl Job {
    pub(super) fn run_warm(
        self,
        worker: &mut LeanWorkerHostHandle,
        actor_healthy: &AtomicBool,
        warm: &mut Option<WarmGeneration>,
    ) -> crate::error::Result<()> {
        if self
            .intent
            .project
            .as_deref()
            .is_none_or(|root| !self.intent.source_matches(root))
        {
            return Err(internal("warm inspection source changed"));
        }
        let (expected, ordinal) = if let Some(prior) = warm.as_ref() {
            if prior.imports != self.intent.imports {
                return Err(internal("warm inspection imports differ from the prepared generation"));
            }
            let next = prior
                .command
                .checked_add(1)
                .ok_or_else(|| internal("warm command overflow"))?;
            (prior.expected.clone(), next)
        } else {
            actor_healthy.store(false, Ordering::Release);
            let original = worker.managed_guardian_binding().map_err(internal)?;
            let expected = worker
                .prepare_managed_artifact_read(self.intent.imports.clone(), self.deadline.expires_ns)
                .map_err(internal)?;
            if expected != original {
                return Err(internal("warm inspection receiver changed during read preparation"));
            }
            (expected, 1)
        };
        if self
            .intent
            .project
            .as_deref()
            .is_none_or(|root| !self.intent.source_matches(root))
        {
            return Err(internal("warm inspection source changed before consume"));
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
        let capability = &self.original.capability;
        let cancellation = LeanWorkerCancellationToken::new();
        let (binding, grant) = {
            let mut store = self.store.lock();
            let scope = Scope {
                id: uuid::Uuid::new_v4().into_bytes(),
                generation: 1,
                owner: uuid::Uuid::new_v4().into_bytes(),
            };
            let binding = store
                .bind_warm_contract(&self.original, scope, &registry, self.deadline.expires_ns, ordinal)
                .map_err(internal)?;
            // From binding onward an interrupted transition must retire this actor.
            actor_healthy.store(false, Ordering::Release);
            store
                .observe_resource(binding, capability, ResourceReport::Open)
                .map_err(internal)?;
            let Consume::Granted(grant) = store.consume_launch(binding, capability).map_err(internal)? else {
                return Err(internal("warm inspection grant was not consumed"));
            };
            let grant = grant
                .into_deadline_grant()
                .ok_or_else(|| internal("warm timed grant missing"))?;
            store
                .attach_active_stop(binding, cancellation.clone())
                .map_err(internal)?;
            drop(store);
            (binding, grant)
        };
        let _stop = StopRegistration {
            store: &self.store,
            binding,
        };
        let (admitted, admitted_registry, expires_ns, intent) = grant.into_parts();
        if admitted != binding
            || admitted_registry != registry
            || expires_ns != self.deadline.expires_ns
            || intent != self.original.intent
        {
            return Err(internal("warm consumed grant differs from the original stage"));
        }
        let command = ManagedInspectionCommand {
            generation: expected.generation.clone(),
            attempt: binding.key.nonce,
            stage: binding.key.stage,
            command: ordinal,
        };
        let finish = CheckedWarmFinish {
            binding,
            registry,
            ordinal,
            expires_ns,
        };
        let outcome = worker
            .inspect_admitted_declaration_warm(
                &self.intent.imports,
                self.request,
                &expected,
                command,
                expires_ns,
                Some(&cancellation),
                |result| {
                    let body = serde_json::to_vec(result).map_err(callback_error)?;
                    self.store
                        .lock()
                        .observe_result(binding, capability, binding.key.nonce, &body)
                        .map_err(callback_error)
                },
                || {
                    self.store
                        .lock()
                        .end_warm_contract(&finish, capability)
                        .map_err(callback_error)
                },
            )
            .map_err(internal)?;
        match outcome {
            ManagedWarmInspectionOutcome::Reusable(_) => {
                *warm = Some(WarmGeneration {
                    expected,
                    imports: self.intent.imports,
                    command: ordinal,
                });
                actor_healthy.store(true, Ordering::Release);
            }
            ManagedWarmInspectionOutcome::Retired(outcome) => {
                if outcome.retirement.is_ok() {
                    self.store
                        .lock()
                        .observe_resource(binding, capability, ResourceReport::Closed)
                        .map_err(internal)?;
                }
            }
        }
        Ok(())
    }
}
