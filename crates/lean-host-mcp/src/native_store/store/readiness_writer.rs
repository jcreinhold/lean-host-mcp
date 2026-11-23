use lean_toolchain::shared_writer::WriterStageReady;
use rusqlite::{TransactionBehavior, params};

use super::{
    AdmissionDeadline, Answer, Binding, Capability, Gate, NativeKey, NativeStore, Problem, Reject, ResourceReport,
    ResultKnowledge, Scope, authenticate, bind_scope_transaction, commit, digest, number, read_stage, reject,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadinessDecision {
    Launch,
    RejectToolingInputs,
}

const TOOLING_INPUT_REFUSAL: &[u8] =
    br#"{"version":1,"stage":"lean-tooling-suites","status":"not_started","reason":"tooling_suite_inputs_unavailable_or_changed","checks":[]}"#;

impl NativeStore {
    /// Atomically record the Ready helper and spend the caller's original
    /// attempt. No uncommitted or ambiguous write can authorize dispatch.
    pub(crate) fn bind_readiness_writer(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        intent: &[u8],
        ready: &WriterStageReady,
        deadline: AdmissionDeadline,
        decision: ReadinessDecision,
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
        if decision == ReadinessDecision::Launch {
            deadline.remaining()?;
        }
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
        let (gate, stop) = match decision {
            ReadinessDecision::Launch => (1, false),
            ReadinessDecision::RejectToolingInputs => (2, true),
        };
        tx.execute(
            "UPDATE stages SET gate=?4,stop_requested=?5,resource_report=1
             WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?, gate, stop],
        )?;
        commit(tx, "consume")?;
        Ok(Some(binding))
    }

    /// The caller must have observed exact closure of the bound preflight writer.
    pub(crate) fn finish_tooling_input_refusal(&mut self, binding: Binding, capability: &Capability) -> Answer<()> {
        self.mutate_bound(binding, capability, |tx, old| {
            if old.gate != Gate::Sealed
                || !old.stop_requested
                || old.spawn.is_some()
                || old.resource_report != ResourceReport::Open
                || old.result != ResultKnowledge::Pending
            {
                return reject(Reject::WrongScope);
            }
            tx.execute(
                "UPDATE stages SET resource_report=3,result_kind=1,result_id=?1,result_hash=?2,result_body=?3
                 WHERE epoch=?4 AND nonce=?5 AND stage=?6",
                params![
                    uuid::Uuid::new_v4().as_bytes(),
                    digest(TOOLING_INPUT_REFUSAL),
                    TOOLING_INPUT_REFUSAL,
                    number(binding.key.epoch)?,
                    binding.key.nonce.as_slice(),
                    number(binding.key.stage)?
                ],
            )?;
            Ok(())
        })
    }
}
