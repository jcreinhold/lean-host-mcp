use lean_toolchain::native_guardian::NativeProcessIdentity;

use rusqlite::{TransactionBehavior, params};

use super::{
    Answer, Binding, Capability, Gate, NativeKey, NativeStore, Problem, Reject, ResourceReport, Scope, authenticate,
    bind_scope_transaction, commit, number, read_stage, recovery, reject,
};

impl NativeStore {
    /// Spend the original admission before any artifact data is read. The
    /// parent-only guard never loans its descriptors to a worker or guardian.
    pub(crate) fn consume_trace_read(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        intent: &[u8],
    ) -> Answer<Option<Binding>> {
        self.checked_key(key)?;
        self.db.check_profile()?;
        let owner = NativeProcessIdentity::capture(std::process::id())
            .ok()
            .flatten()
            .ok_or(Problem::Rejected(Reject::WrongScope))?;
        let identity = serde_json::to_vec(&owner).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authenticate(&tx, key, capability)?;
        let original = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if !original.matches_intent(intent) {
            return reject(Reject::IntentChanged);
        }
        if original.gate != Gate::Pending || original.scope.is_some() {
            return Ok(None);
        }
        original
            .admission_deadline
            .ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))?
            .remaining()?;
        let scope = Scope {
            id: uuid::Uuid::new_v4().into_bytes(),
            generation: 1,
            owner: uuid::Uuid::new_v4().into_bytes(),
        };
        let binding = bind_scope_transaction(&tx, key, capability, scope)?;
        tx.execute(
            "UPDATE stages SET gate=1,resource_report=1,trace_reader=?1 WHERE epoch=?2 AND nonce=?3 AND stage=?4",
            params![identity, number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
        )?;
        commit(tx, "consume")?;
        Ok(Some(binding))
    }

    /// Reconcile this exact parent-only scope. Losing the parent says nothing
    /// about the result and never restores the spent launch gate.
    pub(crate) fn reconcile_trace_read(&mut self, key: NativeKey, capability: &Capability) -> Answer<()> {
        let stage = self.client_get(key, capability)?;
        let Some(scope) = stage.scope else { return Ok(()) };
        if stage.gate != Gate::Consumed || stage.resource_report == ResourceReport::Closed {
            return Ok(());
        }
        let identity: Option<Vec<u8>> = self.db.connection().query_row(
            "SELECT trace_reader FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3 AND scope=?4",
            params![
                number(key.epoch)?,
                key.nonce.as_slice(),
                number(key.stage)?,
                scope.id.as_slice()
            ],
            |row| row.get(0),
        )?;
        let Some(identity) = identity else { return Ok(()) };
        drop(
            serde_json::from_slice::<NativeProcessIdentity>(&identity)
                .map_err(|_| Problem::Rejected(Reject::WrongScope))?,
        );
        let deadline = stage
            .admission_deadline
            .ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))?;
        // A dead broker alone does not rule out transient fork inheritance
        // before exec. Keep unreported same-boot custody unresolved.
        if deadline.boot != recovery::boot_id()? {
            self.observe_resource(Binding { key, scope }, capability, ResourceReport::Closed)?;
        }
        Ok(())
    }
}
