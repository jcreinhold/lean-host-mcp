use lean_toolchain::shared_writer::WriterStageReady;
use rusqlite::{TransactionBehavior, params};

use super::{
    AdmissionDeadline, Answer, Binding, Capability, Gate, NativeKey, NativeStore, Problem, Reject, Scope, authenticate,
    bind_scope_transaction, commit, digest, number, read_stage, reject,
};

impl NativeStore {
    /// Atomically record the Ready helper and spend the caller's original
    /// attempt. No uncommitted or ambiguous write can authorize dispatch.
    pub(crate) fn consume_readiness_writer(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        intent: &[u8],
        ready: &WriterStageReady,
        deadline: AdmissionDeadline,
    ) -> Answer<Option<Binding>> {
        self.checked_key(key)?;
        self.db.check_profile()?;
        let identity = serde_json::to_vec(ready).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        if identity.len() > 512
            || !ready
                .boot()
                .eq_ignore_ascii_case(&uuid::Uuid::from_bytes(deadline.boot).to_string())
        {
            return reject(Reject::WrongScope);
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authenticate(&tx, key, capability)?;
        let stage = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if !stage.matches_intent(intent) {
            return reject(Reject::IntentChanged);
        }
        if stage.gate != Gate::Pending || stage.scope.is_some() {
            return Ok(None);
        }
        if stage.admission_deadline != Some(deadline) {
            return reject(Reject::InvalidLaunchDeadline);
        }
        deadline.remaining()?;
        // Content-address the owner under a fresh scope ID. The entire
        // receipt and its original stage remain independently inspectable.
        let owner = digest(&identity)
            .get(..16)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(Problem::Rejected(Reject::WrongScope))?;
        let binding = bind_scope_transaction(
            &tx,
            key,
            capability,
            Scope {
                id: uuid::Uuid::new_v4().into_bytes(),
                generation: 1,
                owner,
            },
        )?;
        // This table is available only for the matched local SDK build.
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS readiness_writer_bindings(
            epoch INTEGER NOT NULL, nonce BLOB NOT NULL, stage INTEGER NOT NULL,
            helper BLOB NOT NULL CHECK(length(helper) BETWEEN 1 AND 512),
            PRIMARY KEY(epoch,nonce,stage),
            FOREIGN KEY(epoch,nonce,stage) REFERENCES stages(epoch,nonce,stage) ON DELETE CASCADE
        ) STRICT",
        )?;
        tx.execute(
            "INSERT INTO readiness_writer_bindings VALUES(?1,?2,?3,?4)",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?, identity],
        )?;
        tx.execute(
            "UPDATE stages SET gate=1,resource_report=1 WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
        )?;
        commit(tx, "consume")?;
        Ok(Some(binding))
    }
}
