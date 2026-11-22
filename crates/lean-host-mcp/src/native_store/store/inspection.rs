//! Trusted dispatch adapter for an already consumed declaration grant.
use super::{Answer, Binding, Capability, NativeStore, Problem, Reject, recovery::DeadlineLaunchGrant};
use lean_rs_worker_parent::{LeanWorker, LeanWorkerError, ManagedInspectionOutcome};
use lean_toolchain::native_guardian::NativeGuardianDrainBinding;

/// The original result and retirement observation, bound to their consumed stage.
pub(crate) struct ObservedInspection {
    binding: Binding,
    pub(crate) outcome: ManagedInspectionOutcome,
}

impl ObservedInspection {
    /// Save only a received typed result. Retirement is reconciled separately.
    pub(crate) fn save_result(&self, store: &mut NativeStore, capability: &Capability) -> Answer<()> {
        if let Ok(result) = &self.outcome.result {
            let body = serde_json::to_vec(result).map_err(|_| Problem::Rejected(Reject::InvalidResult))?;
            store.observe_result(self.binding, capability, self.binding.key.nonce, &body)?;
        }
        Ok(())
    }
}

/// Move the original private grant into its exact worker. The SDK checks the
/// receiver and waits for the deadline ACK before sending any request bytes.
pub(crate) fn inspect_consumed(
    worker: &mut LeanWorker,
    grant: DeadlineLaunchGrant,
) -> Result<ObservedInspection, LeanWorkerError> {
    let (binding, registry, expires_ns, intent) = grant.into_parts();
    let request = serde_json::from_slice(&intent).map_err(|error| LeanWorkerError::Setup {
        message: format!("invalid admitted declaration inspection: {error}"),
    })?;
    let expected = NativeGuardianDrainBinding {
        registry_device: registry.device,
        registry_inode: registry.inode,
        generation: registry.generation,
        boot: uuid::Uuid::from_bytes(registry.boot).hyphenated().to_string(),
        parent: registry.parent,
        worker: registry.worker,
        guardian: registry.guardian,
    };
    let outcome = worker.inspect_admitted_declaration_once(
        request,
        &expected,
        binding.key.nonce,
        binding.key.stage,
        expires_ns,
    )?;
    Ok(ObservedInspection { binding, outcome })
}
