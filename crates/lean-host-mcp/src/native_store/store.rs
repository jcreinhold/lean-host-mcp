//! Durable native stage state and one-time launch grants.
use super::connection::{CheckedConnection, ConnectionError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{fmt, path::Path};

#[path = "store/boundary.rs"]
pub(crate) mod boundary;
#[cfg(test)]
#[path = "../../tests/stage_store_contract/cuts.rs"]
mod cuts;
#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
pub(crate) mod inspection;
#[path = "store/recovery.rs"]
pub(crate) mod recovery;

const CALLER_ROLE: i64 = 0x4c53_434a;
const NATIVE_ROLE: i64 = 0x4c53_4e53;
const LIMIT: usize = 65_536;
const MAX_STAGES: i64 = 1_024;
type Id = [u8; 16];

#[derive(Debug)]
pub(crate) enum Problem {
    Sql(rusqlite::Error),
    Connection(ConnectionError),
    Rejected(Reject),
}
impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => write!(f, "store SQL failed: {error}"),
            Self::Connection(error) => error.fmt(f),
            Self::Rejected(reason) => write!(f, "store operation rejected: {reason:?}"),
        }
    }
}
impl std::error::Error for Problem {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::Connection(error) => Some(error),
            Self::Rejected(_) => None,
        }
    }
}
impl From<rusqlite::Error> for Problem {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<ConnectionError> for Problem {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::WrongSchema => Self::Rejected(Reject::WrongSchema),
            other @ (ConnectionError::Sqlite(_)
            | ConnectionError::Io(_)
            | ConnectionError::NotAnAbsoluteRegularFile(_)
            | ConnectionError::UnsupportedRuntime(_)
            | ConnectionError::UnexpectedProfile(_)) => Self::Connection(other),
        }
    }
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Reject {
    WrongStore,
    IntentChanged,
    NativeIdentityAlreadyBound,
    CapabilityDenied,
    ClosedEpoch,
    FutureEpoch,
    EpochChanged,
    UnknownStage,
    WrongScope,
    ScopeAlreadyBound,
    StaleGeneration,
    NoConsumedGate,
    InvalidLaunchDeadline,
    ConflictingSpawn,
    ConflictingResult,
    ResultUnknown,
    IdentifierRange,
    Oversized,
    StoreFull,
    WrongSchema,
    UnknownCaller,
    CallerBindingChanged,
    ConflictingCallerResult,
    ResultUnavailable,
    InvalidResult,
    LaunchSealed,
    DispatchDisabled,
    RecoveryClaimBusy,
}
type Answer<T> = Result<T, Problem>;
fn reject<T>(why: Reject) -> Answer<T> {
    Err(Problem::Rejected(why))
}
fn number(n: u64) -> Answer<i64> {
    i64::try_from(n).map_err(|_| Problem::Rejected(Reject::IdentifierRange))
}
fn bounded(value: &[u8]) -> Answer<()> {
    if value.len() > LIMIT {
        reject(Reject::Oversized)
    } else {
        Ok(())
    }
}
fn digest(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}
// Check borrowed SQLite bytes before any Rust-owned BLOB allocation. SQLite's
// own page/value buffers are outside this contract's allocation guarantee.
fn blob<'row>(row: &'row rusqlite::Row<'_>, column: usize) -> rusqlite::Result<&'row [u8]> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Blob(bytes) => {
            if bytes.len() <= LIMIT {
                Ok(bytes)
            } else {
                Err(rusqlite::Error::InvalidQuery)
            }
        }
        rusqlite::types::ValueRef::Null
        | rusqlite::types::ValueRef::Integer(_)
        | rusqlite::types::ValueRef::Real(_)
        | rusqlite::types::ValueRef::Text(_) => Err(rusqlite::Error::InvalidQuery),
    }
}
fn fixed<const N: usize>(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<[u8; N]> {
    blob(row, column)?.try_into().map_err(|_| rusqlite::Error::InvalidQuery)
}
fn optional_fixed<const N: usize>(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<[u8; N]>> {
    if row.get_ref(column)? == rusqlite::types::ValueRef::Null {
        Ok(None)
    } else {
        fixed(row, column).map(Some)
    }
}
fn positive(n: i64) -> rusqlite::Result<u64> {
    u64::try_from(n).map_err(|_| rusqlite::Error::InvalidQuery)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CallerKey {
    pub(crate) job: Vec<u8>,
    pub(crate) stage: u64,
    pub(crate) revision: u64,
}
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeKey {
    pub(crate) store: Id,
    pub(crate) epoch: u64,
    pub(crate) nonce: Id,
    pub(crate) stage: u64,
}
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Capability(pub(crate) [u8; 32]);
impl fmt::Debug for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Capability([redacted])")
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SavedRequest {
    pub(crate) caller: CallerKey,
    pub(crate) key: NativeKey,
    pub(crate) intent: Vec<u8>,
    pub(crate) capability: Capability,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Scope {
    pub(crate) id: Id,
    pub(crate) generation: u64,
    pub(crate) owner: Id,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Binding {
    pub(crate) key: NativeKey,
    pub(crate) scope: Scope,
}
/// A committed/recovered caller copy, not producer authentication or permission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CallerResult {
    caller: CallerKey,
    binding: Binding,
    result: Id,
    hash: Vec<u8>,
    body: Vec<u8>,
}
impl CallerResult {
    pub(crate) fn caller(&self) -> &CallerKey {
        &self.caller
    }
    pub(crate) fn binding(&self) -> Binding {
        self.binding
    }
    pub(crate) fn result_id(&self) -> Id {
        self.result
    }
    pub(crate) fn hash(&self) -> &[u8] {
        &self.hash
    }
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Gate {
    Pending,
    Consumed,
    Sealed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Spawn {
    Spawned,
    Failed,
}
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ResourceReport {
    None,
    Open,
    Stopping,
    Closed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResultKnowledge {
    Pending,
    Retained { id: Id, hash: Vec<u8>, body: Vec<u8> },
    Acknowledged { id: Id, hash: Vec<u8>, body: Vec<u8> },
    Expired { id: Id, hash: Vec<u8> },
}
impl ResultKnowledge {
    fn identity(&self) -> Option<(Id, &[u8])> {
        match self {
            Self::Pending => None,
            Self::Retained { id, hash, .. } | Self::Acknowledged { id, hash, .. } | Self::Expired { id, hash } => {
                Some((*id, hash))
            }
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) key: NativeKey,
    pub(crate) intent: Vec<u8>,
    pub(crate) compacted_intent_sha256: Option<[u8; 32]>,
    pub(crate) scope: Option<Scope>,
    pub(crate) gate: Gate,
    pub(crate) spawn: Option<Spawn>,
    pub(crate) stop_requested: bool,
    pub(crate) resource_report: ResourceReport,
    pub(crate) result: ResultKnowledge,
}
impl Snapshot {
    pub(crate) fn matches_intent(&self, intent: &[u8]) -> bool {
        self.compacted_intent_sha256
            .map_or_else(|| self.intent == intent, |hash| hash.as_slice() == digest(intent))
    }
}

#[derive(Debug)]
pub(crate) struct LaunchPermit {
    binding: Binding,
    deadline: Option<recovery::DeadlineLaunchGrant>,
}
impl LaunchPermit {
    /// Untimed native fixtures cannot silently discard a timed grant's identity.
    pub(crate) fn into_binding(self) -> Option<Binding> {
        self.deadline.is_none().then_some(self.binding)
    }

    /// Transfer the one-time grant to the deadline/launch adapter.
    pub(crate) fn into_deadline_grant(self) -> Option<recovery::DeadlineLaunchGrant> {
        self.deadline
    }
}
#[derive(Debug)]
pub(crate) enum Consume {
    Granted(LaunchPermit),
    AlreadyConsumed,
    Sealed,
}

fn commit(tx: Transaction<'_>, _point: &str) -> Answer<()> {
    #[cfg(test)]
    cuts::cut(&format!("{_point}:before"));
    tx.commit()?;
    #[cfg(test)]
    cuts::cut(&format!("{_point}:after"));
    Ok(())
}
fn has_role(db: &Connection, expected: i64, expected_version: i64) -> rusqlite::Result<bool> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let role: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    Ok(version == expected_version && role == expected)
}
// These bounded metadata reads identify the supported format; no history scan,
// migration or journal-mode write runs before them.
pub(crate) fn caller_schema_valid(db: &Connection) -> rusqlite::Result<bool> {
    if !has_role(db, CALLER_ROLE, 2)? {
        return Ok(false);
    }
    drop(
        db.prepare("SELECT job,stage,revision,store,epoch,nonce,native_stage,intent,capability FROM bindings LIMIT 0")?,
    );
    drop(db.prepare(
        "SELECT job,stage,revision,scope,generation,owner,result_id,result_hash,body FROM accepted_results LIMIT 0",
    )?);
    Ok(true)
}
pub(crate) fn native_schema_valid(db: &Connection) -> rusqlite::Result<bool> {
    if !has_role(db, NATIVE_ROLE, 5)? {
        return Ok(false);
    }
    drop(db.prepare("SELECT epoch,nonce,stage,intent,cap_hash,scope,generation,owner,gate,spawn,stop_requested,resource_report,result_kind,result_id,result_hash,result_body FROM stages LIMIT 0")?);
    drop(db.prepare("SELECT scope,generation,owner FROM scope_heads LIMIT 0")?);
    drop(db.prepare("SELECT epoch,nonce,stage,job,caller_stage,revision,registry_generation,identity,expires_ns FROM recovery_bindings LIMIT 0")?);
    let meta: Option<(Id, i64)> = db
        .query_row("SELECT store,epoch FROM meta WHERE singleton=1", [], |r| {
            Ok((fixed(r, 0)?, r.get(1)?))
        })
        .optional()?;
    Ok(meta.is_some_and(|(_, epoch)| epoch > 0))
}

pub(crate) struct CallerJournal {
    db: CheckedConnection,
}
impl CallerJournal {
    pub(crate) fn create_new(path: &Path) -> Answer<Self> {
        let mut db = CheckedConnection::create_new(path)?;
        let tx = db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!(
            r"PRAGMA application_id={CALLER_ROLE}; PRAGMA user_version=2;
            CREATE TABLE bindings(job BLOB NOT NULL,stage INTEGER NOT NULL,revision INTEGER NOT NULL,
            store BLOB NOT NULL CHECK(length(store)=16),epoch INTEGER NOT NULL,nonce BLOB NOT NULL CHECK(length(nonce)=16),
            native_stage INTEGER NOT NULL,intent BLOB NOT NULL CHECK(length(intent)<=65536),
            capability BLOB NOT NULL CHECK(length(capability)=32),PRIMARY KEY(job,stage,revision),UNIQUE(store,epoch,nonce,native_stage)) STRICT;
            CREATE TABLE accepted_results(job BLOB NOT NULL,stage INTEGER NOT NULL,revision INTEGER NOT NULL,
            scope BLOB NOT NULL CHECK(length(scope)=16),generation INTEGER NOT NULL CHECK(generation>=0),owner BLOB NOT NULL CHECK(length(owner)=16),
            result_id BLOB NOT NULL CHECK(length(result_id)=16),result_hash BLOB NOT NULL CHECK(length(result_hash)=32),body BLOB NOT NULL CHECK(length(body)<=65536),
            PRIMARY KEY(job,stage,revision),FOREIGN KEY(job,stage,revision) REFERENCES bindings(job,stage,revision)) STRICT;"
        ))?;
        tx.commit()?;
        Ok(Self { db })
    }
    pub(crate) fn open(path: &Path) -> Answer<Self> {
        let db = CheckedConnection::open_existing(path, caller_schema_valid)?;
        Ok(Self { db })
    }
    pub(crate) fn get(&self, key: &CallerKey) -> Answer<Option<SavedRequest>> {
        caller_get(self.db.connection(), key)
    }
    pub(crate) fn saved_result(&self, key: &CallerKey) -> Answer<Option<CallerResult>> {
        caller_result_get(self.db.connection(), key)
    }
    /// The source snapshot comes from the original native store, checked by the
    /// client bridge. This does not authenticate a producer or infer OS closure.
    pub(crate) fn save_result(&mut self, request: &SavedRequest, source: &Snapshot) -> Answer<CallerResult> {
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual = caller_get(&tx, &request.caller)?.ok_or(Problem::Rejected(Reject::UnknownCaller))?;
        if actual != *request || source.key != request.key || !source.matches_intent(&request.intent) {
            return reject(Reject::CallerBindingChanged);
        }
        let scope = source.scope.ok_or(Problem::Rejected(Reject::WrongScope))?;
        let binding = Binding {
            key: request.key,
            scope,
        };
        let (result, hash, body) = result_parts(source)?;
        if let Some(saved) = caller_result_get(&tx, &request.caller)? {
            if saved.binding != binding
                || saved.result != result
                || saved.hash != hash
                || body.is_some_and(|bytes| saved.body != bytes)
            {
                return reject(Reject::ConflictingCallerResult);
            }
            return Ok(saved);
        }
        let body = body.ok_or(Problem::Rejected(Reject::ResultUnavailable))?;
        let saved = CallerResult {
            caller: request.caller.clone(),
            binding,
            result,
            hash: hash.to_vec(),
            body: body.to_vec(),
        };
        tx.execute(
            "INSERT INTO accepted_results VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                request.caller.job,
                number(request.caller.stage)?,
                number(request.caller.revision)?,
                scope.id.as_slice(),
                number(scope.generation)?,
                scope.owner.as_slice(),
                result.as_slice(),
                hash,
                body
            ],
        )?;
        commit(tx, "caller-result")?;
        Ok(saved)
    }
    pub(crate) fn bind(
        &mut self,
        key: CallerKey,
        intent: &[u8],
        candidate: NativeKey,
        capability: Capability,
    ) -> Answer<SavedRequest> {
        bounded(&key.job)?;
        bounded(intent)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(previous) = caller_get(&tx, &key)? {
            if previous.intent != intent {
                return reject(Reject::IntentChanged);
            }
            // Candidate discovery can change across a retry. Original binding wins.
            return Ok(previous);
        }
        let candidate_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bindings WHERE store=?1 AND epoch=?2 AND nonce=?3 AND native_stage=?4)",
            params![
                candidate.store.as_slice(),
                number(candidate.epoch)?,
                candidate.nonce.as_slice(),
                number(candidate.stage)?
            ],
            |row| row.get(0),
        )?;
        if candidate_exists {
            return reject(Reject::NativeIdentityAlreadyBound);
        }
        tx.execute(
            "INSERT INTO bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                key.job,
                number(key.stage)?,
                number(key.revision)?,
                candidate.store.as_slice(),
                number(candidate.epoch)?,
                candidate.nonce.as_slice(),
                number(candidate.stage)?,
                intent,
                capability.0.as_slice()
            ],
        )?;
        let saved = SavedRequest {
            caller: key,
            key: candidate,
            intent: intent.to_vec(),
            capability,
        };
        commit(tx, "caller-bind")?;
        Ok(saved)
    }
}
fn caller_get(db: &Connection, key: &CallerKey) -> Answer<Option<SavedRequest>> {
    Ok(db.query_row("SELECT store,epoch,nonce,native_stage,intent,capability FROM bindings WHERE job=?1 AND stage=?2 AND revision=?3",params![key.job,number(key.stage)?,number(key.revision)?],|row| {
        Ok(SavedRequest{caller:key.clone(),key:NativeKey{store:fixed(row,0)?,epoch:positive(row.get(1)?)?,nonce:fixed(row,2)?,stage:positive(row.get(3)?)?},intent:blob(row,4)?.to_vec(),capability:Capability(fixed(row,5)?)})
    }).optional()?)
}

fn result_parts(source: &Snapshot) -> Answer<(Id, &[u8], Option<&[u8]>)> {
    match &source.result {
        ResultKnowledge::Retained { id, hash, body } | ResultKnowledge::Acknowledged { id, hash, body } => {
            bounded(body)?;
            if hash.len() != 32 || *hash != digest(body) {
                return reject(Reject::InvalidResult);
            }
            Ok((*id, hash, Some(body)))
        }
        ResultKnowledge::Expired { id, hash } => {
            if hash.len() != 32 {
                return reject(Reject::InvalidResult);
            }
            Ok((*id, hash, None))
        }
        ResultKnowledge::Pending => reject(Reject::ResultUnavailable),
    }
}
fn caller_result_get(db: &Connection, key: &CallerKey) -> Answer<Option<CallerResult>> {
    let saved=db.query_row("SELECT b.store,b.epoch,b.nonce,b.native_stage,r.scope,r.generation,r.owner,r.result_id,r.result_hash,r.body FROM accepted_results r JOIN bindings b USING(job,stage,revision) WHERE r.job=?1 AND r.stage=?2 AND r.revision=?3",params![key.job,number(key.stage)?,number(key.revision)?],|r|{
        Ok(CallerResult{caller:key.clone(),binding:Binding{key:NativeKey{store:fixed(r,0)?,epoch:positive(r.get(1)?)?,nonce:fixed(r,2)?,stage:positive(r.get(3)?)?},scope:Scope{id:fixed(r,4)?,generation:positive(r.get(5)?)?,owner:fixed(r,6)?}},result:fixed(r,7)?,hash:fixed::<32>(r,8)?.to_vec(),body:blob(r,9)?.to_vec()})
    }).optional()?;
    if saved
        .as_ref()
        .is_some_and(|saved| saved.body.len() > LIMIT || saved.hash.len() != 32 || saved.hash != digest(&saved.body))
    {
        return reject(Reject::InvalidResult);
    }
    Ok(saved)
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
struct ActiveStop {
    scope: Scope,
    token: lean_rs_worker_parent::LeanWorkerCancellationToken,
}

pub(crate) struct NativeStore {
    db: CheckedConnection,
    store: Id,
    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    active_stops: std::collections::HashMap<NativeKey, ActiveStop>,
}
impl NativeStore {
    pub(crate) fn create_new(path: &Path) -> Answer<Self> {
        let mut db = CheckedConnection::create_new(path)?;
        let store = *uuid::Uuid::new_v4().as_bytes();
        let tx = db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!("PRAGMA application_id={NATIVE_ROLE}; PRAGMA user_version=5;\n\
            CREATE TABLE meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),store BLOB NOT NULL CHECK(length(store)=16),epoch INTEGER NOT NULL CHECK(epoch>0)) STRICT;\n\
            CREATE TABLE scope_heads(scope BLOB PRIMARY KEY CHECK(length(scope)=16),generation INTEGER NOT NULL CHECK(generation>=0),owner BLOB NOT NULL CHECK(length(owner)=16)) STRICT;\n\
            CREATE TABLE stages(epoch INTEGER NOT NULL,nonce BLOB NOT NULL CHECK(length(nonce)=16),stage INTEGER NOT NULL,\n\
            intent BLOB NOT NULL CHECK(length(intent)<=65536),cap_hash BLOB NOT NULL CHECK(length(cap_hash)=32),\n\
            scope BLOB,generation INTEGER,owner BLOB,gate INTEGER NOT NULL DEFAULT 0 CHECK(gate IN(0,1,2)),\n\
            spawn INTEGER CHECK(spawn IN(1,2)),stop_requested INTEGER NOT NULL DEFAULT 0 CHECK(stop_requested IN(0,1)),\n\
            resource_report INTEGER NOT NULL DEFAULT 0 CHECK(resource_report IN(0,1,2,3)),\n\
            result_kind INTEGER NOT NULL DEFAULT 0 CHECK(result_kind IN(0,1,2,3)),result_id BLOB,result_hash BLOB,result_body BLOB CHECK(length(result_body)<=65536),\n\
            managed_ack INTEGER NOT NULL DEFAULT 0 CHECK(managed_ack IN(0,1)),\n\
            compacted_intent_sha256 BLOB CHECK(length(compacted_intent_sha256)=32),\n\
            drain_auth_hash BLOB CHECK(length(drain_auth_hash)=32),\n\
            recovery_claim_hash BLOB CHECK(length(recovery_claim_hash)=32),\n\
            PRIMARY KEY(epoch,nonce,stage),UNIQUE(scope,generation),\n\
            CHECK((scope IS NULL AND generation IS NULL AND owner IS NULL) OR (scope IS NOT NULL AND generation IS NOT NULL AND owner IS NOT NULL AND length(scope)=16 AND generation>=0 AND length(owner)=16))) STRICT;"))?;
        tx.execute_batch(recovery::SCHEMA)?;
        tx.execute("INSERT INTO meta VALUES(1,?1,1)", [store.as_slice()])?;
        tx.commit()?;
        Ok(Self {
            db,
            store,
            #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
            active_stops: std::collections::HashMap::new(),
        })
    }
    pub(crate) fn open(path: &Path) -> Answer<Self> {
        let mut db = CheckedConnection::open_existing(path, |db| {
            if !native_schema_valid(db)? {
                return Ok(false);
            }
            let integrity: String = db.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
            Ok(integrity == "ok")
        })?;
        let tx = db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_ack: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('stages') WHERE name='managed_ack')",
            [],
            |row| row.get(0),
        )?;
        if !has_ack {
            // Older expired rows lack affirmative ACK evidence. Keep them fenced.
            tx.execute_batch(
                "ALTER TABLE stages ADD COLUMN managed_ack INTEGER NOT NULL DEFAULT 0 CHECK(managed_ack IN(0,1))",
            )?;
        }
        let has_intent_digest: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('stages') WHERE name='compacted_intent_sha256')",
            [],
            |row| row.get(0),
        )?;
        if !has_intent_digest {
            tx.execute_batch(
                "ALTER TABLE stages ADD COLUMN compacted_intent_sha256 BLOB CHECK(length(compacted_intent_sha256)=32)",
            )?;
        }
        let has_drain_auth: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('stages') WHERE name='drain_auth_hash')",
            [],
            |row| row.get(0),
        )?;
        if !has_drain_auth {
            tx.execute_batch("ALTER TABLE stages ADD COLUMN drain_auth_hash BLOB CHECK(length(drain_auth_hash)=32)")?;
        }
        let has_claim: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('stages') WHERE name='recovery_claim_hash')",
            [],
            |row| row.get(0),
        )?;
        if !has_claim {
            tx.execute_batch(
                "ALTER TABLE stages ADD COLUMN recovery_claim_hash BLOB CHECK(length(recovery_claim_hash)=32)",
            )?;
        }
        let store = tx.query_row("SELECT store FROM meta WHERE singleton=1", [], |r| fixed(r, 0))?;
        let prior = {
            let mut query = tx.prepare(
                "SELECT epoch,nonce,stage FROM stages
                 WHERE managed_ack=1 AND resource_report=3 AND result_kind=3
                   AND compacted_intent_sha256 IS NULL LIMIT ?1",
            )?;
            query
                .query_map([MAX_STAGES], |r| {
                    Ok(NativeKey {
                        store,
                        epoch: positive(r.get(0)?)?,
                        nonce: fixed(r, 1)?,
                        stage: positive(r.get(2)?)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for key in prior {
            compact_closed_acknowledged(&tx, key)?;
        }
        tx.commit()?;
        Ok(Self {
            db,
            store,
            #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
            active_stops: std::collections::HashMap::new(),
        })
    }
    pub(crate) fn identity(&self) -> Id {
        self.store
    }
    pub(crate) fn epoch(&self) -> Answer<u64> {
        Ok(positive(self.db.connection().query_row(
            "SELECT epoch FROM meta WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)?)
    }
    fn checked_key(&self, key: NativeKey) -> Answer<()> {
        if key.store == self.store {
            Ok(())
        } else {
            reject(Reject::WrongStore)
        }
    }
    // Trusted fixture inspection only; untrusted GET uses boundary::decode/client.
    pub(crate) fn get(&self, key: NativeKey) -> Answer<Option<Snapshot>> {
        self.checked_key(key)?;
        read_stage(self.db.connection(), key)
    }
    pub(crate) fn create(&mut self, saved: &SavedRequest) -> Answer<Snapshot> {
        self.create_intent(saved.key, &saved.capability, &saved.intent, None)
    }
    fn create_intent(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        intent: &[u8],
        drain_auth: Option<[u8; 32]>,
    ) -> Answer<Snapshot> {
        self.checked_key(key)?;
        bounded(intent)?;
        if drain_auth.is_some_and(|secret| secret == [0; 32] || secret == capability.0) {
            return reject(Reject::CapabilityDenied);
        }
        let drain_hash = drain_auth.map(|secret| digest(&secret));
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3)",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
            |row| row.get(0),
        )?;
        if exists {
            authenticate(&tx, key, capability)?;
            if let Some(hash) = drain_hash.as_deref() {
                let original = tx.query_row(
                    "SELECT drain_auth_hash FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",
                    params![number(key.epoch)?, key.nonce, number(key.stage)?],
                    |row| optional_fixed::<32>(row, 0),
                )?;
                if original.as_ref().map(<[u8; 32]>::as_slice) != Some(hash) {
                    return reject(Reject::CapabilityDenied);
                }
            }
            let previous = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
            if !previous.matches_intent(intent) {
                return reject(Reject::IntentChanged);
            }
            return Ok(previous);
        }
        let epoch: u64 = positive(tx.query_row("SELECT epoch FROM meta WHERE singleton=1", [], |r| r.get(0))?)?;
        if key.epoch < epoch {
            return reject(Reject::ClosedEpoch);
        }
        if key.epoch > epoch {
            return reject(Reject::FutureEpoch);
        }
        if tx.query_row("SELECT count(*) FROM stages", [], |row| row.get::<_, i64>(0))? >= MAX_STAGES {
            return reject(Reject::StoreFull);
        }
        tx.execute(
            "INSERT INTO stages(epoch,nonce,stage,intent,cap_hash,drain_auth_hash) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                number(key.epoch)?,
                key.nonce.as_slice(),
                number(key.stage)?,
                intent,
                digest(&capability.0),
                drain_hash
            ],
        )?;
        let snapshot = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        commit(tx, "native-create")?;
        Ok(snapshot)
    }
    pub(crate) fn bind_scope(&mut self, key: NativeKey, capability: &Capability, scope: Scope) -> Answer<Binding> {
        self.checked_key(key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding = bind_scope_transaction(&tx, key, capability, scope)?;
        tx.commit()?;
        Ok(binding)
    }

    pub(crate) fn consume_launch(&mut self, binding: Binding, capability: &Capability) -> Answer<Consume> {
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = bound_stage(&tx, binding, capability)?;
        match old.gate {
            Gate::Consumed => return Ok(Consume::AlreadyConsumed),
            Gate::Sealed => return Ok(Consume::Sealed),
            Gate::Pending => {}
        }
        let current: (i64, Id) = tx.query_row(
            "SELECT generation,owner FROM scope_heads WHERE scope=?1",
            [binding.scope.id.as_slice()],
            |r| Ok((r.get(0)?, fixed(r, 1)?)),
        )?;
        if current != (number(binding.scope.generation)?, binding.scope.owner) {
            return reject(Reject::StaleGeneration);
        }
        let deadline = recovery::launch_deadline(&tx, binding, old.intent)?;
        let changed = tx.execute(
            "UPDATE stages SET gate=1 WHERE epoch=?1 AND nonce=?2 AND stage=?3 AND gate=0",
            params![
                number(binding.key.epoch)?,
                binding.key.nonce.as_slice(),
                number(binding.key.stage)?
            ],
        )?;
        if changed != 1 {
            return reject(Reject::NoConsumedGate);
        }
        commit(tx, "consume")?;
        Ok(Consume::Granted(LaunchPermit { binding, deadline }))
    }
    // The store mutex covers consumption, registration and committed cancellation delivery.
    // This entry routes a stop to the actor; it grants neither launch nor closure.
    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn attach_active_stop(
        &mut self,
        binding: Binding,
        token: lean_rs_worker_parent::LeanWorkerCancellationToken,
    ) -> Answer<()> {
        use std::collections::hash_map::Entry;
        match self.active_stops.entry(binding.key) {
            Entry::Vacant(entry) => {
                entry.insert(ActiveStop {
                    scope: binding.scope,
                    token,
                });
                Ok(())
            }
            Entry::Occupied(_) => reject(Reject::WrongScope),
        }
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn detach_active_stop(&mut self, binding: Binding) {
        use std::collections::hash_map::Entry;
        if let Entry::Occupied(entry) = self.active_stops.entry(binding.key)
            && entry.get().scope == binding.scope
        {
            entry.remove();
        }
    }

    pub(crate) fn cancel(&mut self, binding: Binding, capability: &Capability) -> Answer<()> {
        self.cancel_original(binding.key, capability, Some(binding.scope))
            .map(|_| ())
    }
    fn cancel_original(
        &mut self,
        key: NativeKey,
        capability: &Capability,
        expected: Option<Scope>,
    ) -> Answer<Snapshot> {
        self.checked_key(key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authenticate(&tx, key, capability)?;
        let old = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if expected.is_some_and(|scope| old.scope != Some(scope)) {
            return reject(Reject::WrongScope);
        }
        let gate = if old.gate == Gate::Pending {
            Gate::Sealed
        } else {
            old.gate
        };
        tx.execute(
            "UPDATE stages SET gate=?1,stop_requested=1 WHERE epoch=?2 AND nonce=?3 AND stage=?4",
            params![
                gate_code(gate),
                number(key.epoch)?,
                key.nonce.as_slice(),
                number(key.stage)?
            ],
        )?;
        let snapshot = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        commit(tx, "cancel")?;
        #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
        if snapshot.gate == Gate::Consumed
            && snapshot.resource_report < ResourceReport::Closed
            && let Some(active) = self.active_stops.get(&snapshot.key)
            && Some(active.scope) == snapshot.scope
        {
            active.token.cancel();
        }
        Ok(snapshot)
    }
    // Trusted internal observer: capability checks do NOT authenticate a producer.
    pub(crate) fn observe_resource(
        &mut self,
        binding: Binding,
        capability: &Capability,
        report: ResourceReport,
    ) -> Answer<()> {
        self.mutate_bound(binding, capability, |tx, old| {
            let report = old.resource_report.max(report);
            let gate = if report >= ResourceReport::Stopping && old.gate == Gate::Pending {
                2
            } else {
                gate_code(old.gate)
            };
            tx.execute(
                "UPDATE stages SET resource_report=?1,gate=?2,
                 result_body=CASE WHEN ?1=3 AND result_kind=2 THEN NULL ELSE result_body END,
                 result_kind=CASE WHEN ?1=3 AND result_kind=2 THEN 3 ELSE result_kind END
                 WHERE epoch=?3 AND nonce=?4 AND stage=?5",
                params![
                    report_code(report),
                    gate,
                    number(binding.key.epoch)?,
                    binding.key.nonce.as_slice(),
                    number(binding.key.stage)?
                ],
            )?;
            if report == ResourceReport::Closed {
                compact_closed_acknowledged(tx, binding.key)?;
            }
            Ok(())
        })
    }
    pub(crate) fn observe_spawn(&mut self, binding: Binding, capability: &Capability, spawn: Spawn) -> Answer<()> {
        self.mutate_bound(binding, capability, |tx, old| {
            if old.gate != Gate::Consumed {
                return reject(Reject::NoConsumedGate);
            }
            if old.spawn.is_some_and(|s| s != spawn) {
                return reject(Reject::ConflictingSpawn);
            }
            tx.execute(
                "UPDATE stages SET spawn=?1 WHERE epoch=?2 AND nonce=?3 AND stage=?4",
                params![
                    if spawn == Spawn::Spawned { 1 } else { 2 },
                    number(binding.key.epoch)?,
                    binding.key.nonce.as_slice(),
                    number(binding.key.stage)?
                ],
            )?;
            Ok(())
        })
    }
    pub(crate) fn observe_result(
        &mut self,
        binding: Binding,
        capability: &Capability,
        result: Id,
        body: &[u8],
    ) -> Answer<()> {
        bounded(body)?;
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = bound_stage(&tx, binding, capability)?;
        let hash = digest(body);
        if let Some((previous, previous_hash)) = old.result.identity() {
            if previous != result || previous_hash != hash {
                return reject(Reject::ConflictingResult);
            }
            return Ok(());
        }
        tx.execute("UPDATE stages SET result_kind=1,result_id=?1,result_hash=?2,result_body=?3 WHERE epoch=?4 AND nonce=?5 AND stage=?6",params![result.as_slice(),hash,body,number(binding.key.epoch)?,binding.key.nonce.as_slice(),number(binding.key.stage)?])?;
        commit(tx, "result")?;
        Ok(())
    }
    pub(crate) fn ack(&mut self, binding: Binding, capability: &Capability, result: Id) -> Answer<()> {
        self.retain_result(binding, capability, result, false)
    }
    pub(crate) fn expire(&mut self, binding: Binding, capability: &Capability, result: Id) -> Answer<()> {
        self.retain_result(binding, capability, result, true)
    }
    fn retain_result(&mut self, binding: Binding, capability: &Capability, result: Id, expire: bool) -> Answer<()> {
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = bound_stage(&tx, binding, capability)?;
        let Some((previous, _)) = old.result.identity() else {
            return reject(Reject::ResultUnknown);
        };
        if previous != result {
            return reject(Reject::ConflictingResult);
        }
        let kind = if expire || matches!(old.result, ResultKnowledge::Expired { .. }) {
            3
        } else {
            2
        };
        tx.execute("UPDATE stages SET result_kind=?1,result_body=CASE WHEN ?1=3 THEN NULL ELSE result_body END WHERE epoch=?2 AND nonce=?3 AND stage=?4",params![kind,number(binding.key.epoch)?,binding.key.nonce.as_slice(),number(binding.key.stage)?])?;
        if expire {
            tx.commit()?;
        } else {
            commit(tx, "native-ack")?;
        }
        Ok(())
    }
    fn mutate_bound(
        &mut self,
        binding: Binding,
        capability: &Capability,
        change: impl FnOnce(&Transaction<'_>, Snapshot) -> Answer<()>,
    ) -> Answer<()> {
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = bound_stage(&tx, binding, capability)?;
        change(&tx, old)?;
        tx.commit()?;
        Ok(())
    }
    /// Capacity-pressure maintenance. Close the current acceptance epoch before
    /// deleting an acknowledged, closed, expired row from that epoch.
    fn reclaim_spent_stages(&mut self) -> Answer<u64> {
        const SPENT: &str = "managed_ack=1 AND gate<>0 AND resource_report=3 AND result_kind=3 AND result_body IS NULL";
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch: i64 = tx.query_row("SELECT epoch FROM meta WHERE singleton=1", [], |r| r.get(0))?;
        let full = tx.query_row("SELECT count(*) FROM stages", [], |row| row.get::<_, i64>(0))? >= MAX_STAGES;
        let mut next = epoch;
        if full
            && tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM stages WHERE {SPENT} AND epoch=?1)"),
                [epoch],
                |r| r.get::<_, bool>(0),
            )?
        {
            next = epoch.checked_add(1).ok_or(Problem::Rejected(Reject::IdentifierRange))?;
            tx.execute("UPDATE meta SET epoch=?1 WHERE singleton=1", [next])?;
        }
        if full {
            tx.execute(
                &format!(
                    "DELETE FROM recovery_bindings WHERE (epoch,nonce,stage) IN
                 (SELECT epoch,nonce,stage FROM stages WHERE {SPENT} AND epoch<?1)"
                ),
                [next],
            )?;
            let removed = tx.execute(&format!("DELETE FROM stages WHERE {SPENT} AND epoch<?1"), [next])?;
            if removed != 0 {
                tx.execute("DELETE FROM scope_heads WHERE NOT EXISTS(SELECT 1 FROM stages WHERE stages.scope=scope_heads.scope)", [])?;
            }
        }
        commit(tx, "reclaim-spent")?;
        Ok(positive(next)?)
    }

    pub(crate) fn rotate_epoch(&mut self, expected: u64) -> Answer<u64> {
        self.db.check_profile()?;
        let next = number(
            expected
                .checked_add(1)
                .ok_or(Problem::Rejected(Reject::IdentifierRange))?,
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE meta SET epoch=?1 WHERE singleton=1 AND epoch=?2",
            params![next, number(expected)?],
        )? != 1
        {
            return reject(Reject::EpochChanged);
        }
        tx.commit()?;
        u64::try_from(next).map_err(|_| Problem::Rejected(Reject::IdentifierRange))
    }
    #[cfg(test)]
    pub(crate) fn limit_growth_for_test(&self) -> Answer<()> {
        let db = self.db.connection();
        let pages: i64 = db.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        db.pragma_update(None, "max_page_count", pages)?;
        let limit: i64 = db.query_row("PRAGMA max_page_count", [], |row| row.get(0))?;
        assert_eq!(limit, pages);
        Ok(())
    }

    pub(crate) fn checkpoint(&self) -> Answer<()> {
        self.db.check_profile()?;
        let (busy, _, _): (i64, i64, i64) =
            self.db
                .connection()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?;
        if busy != 0 {
            return Err(Problem::Sql(rusqlite::Error::InvalidQuery));
        }
        Ok(())
    }
}
fn bind_scope_transaction(
    tx: &Transaction<'_>,
    key: NativeKey,
    capability: &Capability,
    scope: Scope,
) -> Answer<Binding> {
    authenticate(tx, key, capability)?;
    let old = read_stage(tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
    if let Some(previous) = old.scope {
        return if previous == scope {
            Ok(Binding { key, scope })
        } else {
            reject(Reject::ScopeAlreadyBound)
        };
    }
    if old.gate == Gate::Sealed {
        return reject(Reject::LaunchSealed);
    }
    let highest: Option<i64> = tx
        .query_row(
            "SELECT generation FROM scope_heads WHERE scope=?1",
            [scope.id.as_slice()],
            |r| r.get(0),
        )
        .optional()?;
    if highest.is_some_and(|n| number(scope.generation).is_ok_and(|g| g <= n)) {
        return reject(Reject::StaleGeneration);
    }
    let generation = number(scope.generation)?;
    tx.execute("INSERT INTO scope_heads(scope,generation,owner) VALUES(?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET generation=excluded.generation,owner=excluded.owner",params![scope.id.as_slice(),generation,scope.owner.as_slice()])?;
    tx.execute(
        "UPDATE stages SET scope=?1,generation=?2,owner=?3 WHERE epoch=?4 AND nonce=?5 AND stage=?6",
        params![
            scope.id.as_slice(),
            generation,
            scope.owner.as_slice(),
            number(key.epoch)?,
            key.nonce.as_slice(),
            number(key.stage)?
        ],
    )?;
    Ok(Binding { key, scope })
}
fn gate_code(gate: Gate) -> i64 {
    match gate {
        Gate::Pending => 0,
        Gate::Consumed => 1,
        Gate::Sealed => 2,
    }
}
fn report_code(report: ResourceReport) -> i64 {
    match report {
        ResourceReport::None => 0,
        ResourceReport::Open => 1,
        ResourceReport::Stopping => 2,
        ResourceReport::Closed => 3,
    }
}
fn authenticate(db: &Connection, key: NativeKey, capability: &Capability) -> Answer<()> {
    let hash: Option<[u8; 32]> = db
        .query_row(
            "SELECT cap_hash FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
            |r| fixed(r, 0),
        )
        .optional()?;
    match hash {
        None => reject(Reject::UnknownStage),
        Some(hash) if hash.as_slice() != digest(&capability.0) => reject(Reject::CapabilityDenied),
        Some(_) => Ok(()),
    }
}
fn bound_stage(db: &Connection, binding: Binding, capability: &Capability) -> Answer<Snapshot> {
    authenticate(db, binding.key, capability)?;
    let old = read_stage(db, binding.key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
    if old.scope != Some(binding.scope) {
        return reject(Reject::WrongScope);
    }
    Ok(old)
}
fn compact_closed_acknowledged(tx: &Transaction<'_>, key: NativeKey) -> Answer<()> {
    let identity = params![number(key.epoch)?, key.nonce, number(key.stage)?];
    let hash = tx
        .query_row(
            "SELECT intent FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3
           AND resource_report=3 AND result_kind=3 AND managed_ack=1
           AND compacted_intent_sha256 IS NULL",
            identity,
            |row| Ok(digest(blob(row, 0)?)),
        )
        .optional()?;
    if let Some(hash) = hash {
        tx.execute(
            "UPDATE stages SET intent=x'', compacted_intent_sha256=?4 WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce, number(key.stage)?, hash],
        )?;
    }
    Ok(())
}

fn read_stage(db: &Connection, key: NativeKey) -> Answer<Option<Snapshot>> {
    let snapshot = db.query_row("SELECT intent,scope,generation,owner,gate,spawn,stop_requested,resource_report,result_kind,result_id,result_hash,result_body,compacted_intent_sha256,managed_ack FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",params![number(key.epoch)?,key.nonce.as_slice(),number(key.stage)?],|r| {
        let scope = match (optional_fixed(r,1)?,r.get::<_,Option<i64>>(2)?,optional_fixed(r,3)?) {
            (None,None,None) => None,
            (Some(id),Some(generation),Some(owner)) => Some(Scope{id,generation:positive(generation)?,owner}),
            _ => return Err(rusqlite::Error::InvalidQuery),
        };
        let gate=match r.get::<_,i64>(4)? {0=>Gate::Pending,1=>Gate::Consumed,2=>Gate::Sealed,_=>return Err(rusqlite::Error::InvalidQuery)};
        let spawn=match r.get::<_,Option<i64>>(5)? {None=>None,Some(1)=>Some(Spawn::Spawned),Some(2)=>Some(Spawn::Failed),_=>return Err(rusqlite::Error::InvalidQuery)};
        let stop_requested=match r.get::<_,i64>(6)? {0=>false,1=>true,_=>return Err(rusqlite::Error::InvalidQuery)};
        let resource_report=match r.get::<_,i64>(7)? {0=>ResourceReport::None,1=>ResourceReport::Open,2=>ResourceReport::Stopping,3=>ResourceReport::Closed,_=>return Err(rusqlite::Error::InvalidQuery)};
        let result=match r.get::<_,i64>(8)? {
            0 if (9..=11).all(|column| r.get_ref(column).is_ok_and(|value| value==rusqlite::types::ValueRef::Null)) => ResultKnowledge::Pending,
            kind @ 1..=3=>{
                let id=fixed(r,9)?; let hash=fixed::<32>(r,10)?.to_vec();
                match kind {
                    1=>ResultKnowledge::Retained{id,hash,body:blob(r,11)?.to_vec()},
                    2=>ResultKnowledge::Acknowledged{id,hash,body:blob(r,11)?.to_vec()},
                    _ if r.get_ref(11)? == rusqlite::types::ValueRef::Null => ResultKnowledge::Expired{id,hash},
                    _=>return Err(rusqlite::Error::InvalidQuery),
                }
            }
            _=>return Err(rusqlite::Error::InvalidQuery),
        };
        if (scope.is_none() && (gate==Gate::Consumed || spawn.is_some() || resource_report!=ResourceReport::None || result!=ResultKnowledge::Pending))
            || (spawn.is_some() && gate!=Gate::Consumed)
            || (gate==Gate::Pending && (stop_requested || resource_report>=ResourceReport::Stopping)) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let intent = blob(r,0)?;
        let compacted_intent_sha256 = optional_fixed(r,12)?;
        if compacted_intent_sha256.is_some() && (!intent.is_empty()
            || resource_report != ResourceReport::Closed || !matches!(result, ResultKnowledge::Expired { .. })
            || r.get::<_,i64>(13)? != 1) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(Snapshot{key,intent:intent.to_vec(),compacted_intent_sha256,scope,gate,spawn,stop_requested,resource_report,result})
    }).optional()?;
    if let Some(snapshot) = &snapshot {
        match snapshot.result {
            ResultKnowledge::Pending => {}
            ResultKnowledge::Retained { .. }
            | ResultKnowledge::Acknowledged { .. }
            | ResultKnowledge::Expired { .. } => {
                result_parts(snapshot)?;
            }
        }
    }
    Ok(snapshot)
}
