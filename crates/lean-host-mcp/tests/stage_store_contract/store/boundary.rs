//! Private, inline-only client contract. No listener, dispatch or producer bootstrap.
//!
//! Only this DTO is decoded from untrusted client bytes. Scope bindings and the
//! trusted fixture's observe_* calls have no wire representation here. This is
//! authority separation, not qualification of native observation authentication.
#![allow(
    clippy::empty_enums,
    reason = "serde generates an empty field enum for strict Discover {}; unit variants accept forged fields"
)]

use super::{
    Answer, Capability, NativeKey, NativeStore, Problem, Reject, ResultKnowledge, Snapshot, authenticate, commit,
    number, read_stage, reject,
};
use rusqlite::{TransactionBehavior, params};
use serde::Deserialize;

// Allows a maximum inline intent encoded as JSON byte-array plus fixed metadata.
// This private-test bound is not a promised production transport encoding.
const MAX_REQUEST_BYTES: usize = super::LIMIT * 4 + 4096;
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodeError {
    Oversized,
    Malformed,
    Version,
}
// Only decode constructs a checked request; callers cannot bypass its byte,
// version and inline limits by invoking Deserialize directly on ClientRequest.
pub(crate) struct ClientRequest {
    command: ClientCommand,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    version: u8,
    command: ClientCommand,
}
impl std::fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientRequest([redacted])")
    }
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ClientCommand {
    Discover {},
    Create {
        key: NativeKey,
        capability: [u8; 32],
        intent: Vec<u8>,
    },
    Get {
        key: NativeKey,
        capability: [u8; 32],
    },
    Start {
        key: NativeKey,
        capability: [u8; 32],
    },
    Cancel {
        key: NativeKey,
        capability: [u8; 32],
    },
    Ack {
        key: NativeKey,
        capability: [u8; 32],
        result_id: [u8; 16],
        result_digest: [u8; 32],
    },
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ClientReply {
    Discovery { store: [u8; 16], acceptance_epoch: u64 },
    Snapshot(Snapshot),
}
pub(crate) fn decode(bytes: &[u8]) -> Result<ClientRequest, DecodeError> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(DecodeError::Oversized);
    }
    // Do not return serde's input-bearing diagnostics: they can contain secrets.
    let request: WireRequest = serde_json::from_slice(bytes).map_err(|_| DecodeError::Malformed)?;
    if request.version != 1 {
        return Err(DecodeError::Version);
    }
    if matches!(&request.command, ClientCommand::Create { intent, .. } if intent.len() > super::LIMIT) {
        return Err(DecodeError::Oversized);
    }
    Ok(ClientRequest {
        command: request.command,
    })
}
impl NativeStore {
    pub(crate) fn client(&mut self, request: ClientRequest) -> Answer<ClientReply> {
        match request.command {
            ClientCommand::Discover {} => Ok(ClientReply::Discovery {
                store: self.identity(),
                acceptance_epoch: self.epoch()?,
            }),
            ClientCommand::Create {
                key,
                capability,
                intent,
            } => self
                .create_intent(key, &Capability(capability), &intent)
                .map(ClientReply::Snapshot),
            ClientCommand::Get { key, capability } => {
                self.client_get(key, &Capability(capability)).map(ClientReply::Snapshot)
            }
            ClientCommand::Start { key, capability } => {
                self.client_get(key, &Capability(capability))?;
                reject(Reject::DispatchDisabled)
            }
            ClientCommand::Cancel { key, capability } => self
                .cancel_original(key, &Capability(capability), None)
                .map(ClientReply::Snapshot),
            ClientCommand::Ack {
                key,
                capability,
                result_id,
                result_digest,
            } => self
                .client_ack(key, &Capability(capability), result_id, &result_digest)
                .map(ClientReply::Snapshot),
        }
    }
    fn client_get(&mut self, key: NativeKey, capability: &Capability) -> Answer<Snapshot> {
        self.checked_key(key)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        authenticate(&tx, key, capability)?;
        read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))
    }
    fn client_ack(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        result: [u8; 16],
        hash: &[u8; 32],
    ) -> Answer<Snapshot> {
        self.checked_key(key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authenticate(&tx, key, capability)?;
        let old = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        let Some((original, original_hash)) = old.result.identity() else {
            return reject(Reject::ResultUnknown);
        };
        if original != result || original_hash != hash {
            return reject(Reject::ConflictingResult);
        }
        let kind = if matches!(old.result, ResultKnowledge::Expired { .. }) {
            3
        } else {
            2
        };
        tx.execute(
            "UPDATE stages SET result_kind=?1 WHERE epoch=?2 AND nonce=?3 AND stage=?4",
            params![kind, number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
        )?;
        let snapshot = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        commit(tx, "native-ack")?;
        Ok(snapshot)
    }
}

// ClientRequest deliberately has no producer-report variants or Deserialize path
// to Binding/Scope/Spawn/ResourceReport. The direct fixture methods stay private
// trusted inputs. No raw client message can return a LaunchPermit.
