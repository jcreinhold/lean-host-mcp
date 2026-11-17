//! Isolated SQL state prototype. No process launch, custody or admission API.
use crate::connection::{CheckedConnection, ConnectionError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{fmt, path::Path};

const CALLER_ROLE: i64 = 0x4c53_434a;
const NATIVE_ROLE: i64 = 0x4c53_4e53;
const LIMIT: usize = 65_536;
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
    ConflictingSpawn,
    ConflictingResult,
    ResultUnknown,
    IdentifierRange,
    Oversized,
    WrongSchema,
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
fn id(bytes: Vec<u8>) -> rusqlite::Result<Id> {
    bytes.try_into().map_err(|_| rusqlite::Error::InvalidQuery)
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    pub(crate) scope: Option<Scope>,
    pub(crate) gate: Gate,
    pub(crate) spawn: Option<Spawn>,
    pub(crate) stop_requested: bool,
    pub(crate) resource_report: ResourceReport,
    pub(crate) result: ResultKnowledge,
}
#[derive(Debug)]
pub(crate) struct LaunchPermit {
    binding: Binding,
}
impl LaunchPermit {
    pub(crate) fn into_binding(self) -> Binding {
        self.binding
    }
}
#[derive(Debug)]
pub(crate) enum Consume {
    Granted(LaunchPermit),
    AlreadyConsumed,
    Sealed,
}

fn commit(tx: Transaction<'_>, point: &str) -> Answer<()> {
    crate::cut(&format!("{point}:before"));
    tx.commit()?;
    crate::cut(&format!("{point}:after"));
    Ok(())
}
fn has_role(db: &Connection, expected: i64) -> rusqlite::Result<bool> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let role: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    Ok(version == 1 && role == expected)
}
// These bounded metadata reads identify the supported format; no history scan,
// migration or journal-mode write runs before them.
pub(crate) fn caller_schema_valid(db: &Connection) -> rusqlite::Result<bool> {
    if !has_role(db, CALLER_ROLE)? {
        return Ok(false);
    }
    drop(
        db.prepare("SELECT job,stage,revision,store,epoch,nonce,native_stage,intent,capability FROM bindings LIMIT 0")?,
    );
    Ok(true)
}
pub(crate) fn native_schema_valid(db: &Connection) -> rusqlite::Result<bool> {
    if !has_role(db, NATIVE_ROLE)? {
        return Ok(false);
    }
    drop(db.prepare("SELECT epoch,nonce,stage,intent,cap_hash,scope,generation,owner,gate,spawn,stop_requested,resource_report,result_kind,result_id,result_hash,result_body FROM stages LIMIT 0")?);
    drop(db.prepare("SELECT scope,generation,owner FROM scope_heads LIMIT 0")?);
    let meta: Option<(Vec<u8>, i64)> = db
        .query_row("SELECT store,epoch FROM meta WHERE singleton=1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    Ok(meta.is_some_and(|(store, epoch)| store.len() == 16 && epoch > 0))
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
        tx.execute_batch(&format!("PRAGMA application_id={CALLER_ROLE}; PRAGMA user_version=1;\n\
            CREATE TABLE bindings(job BLOB NOT NULL,stage INTEGER NOT NULL,revision INTEGER NOT NULL,\n\
            store BLOB NOT NULL CHECK(length(store)=16),epoch INTEGER NOT NULL,nonce BLOB NOT NULL CHECK(length(nonce)=16),\n\
            native_stage INTEGER NOT NULL,intent BLOB NOT NULL CHECK(length(intent)<=65536),\n\
            capability BLOB NOT NULL CHECK(length(capability)=32),PRIMARY KEY(job,stage,revision),UNIQUE(store,epoch,nonce,native_stage)) STRICT;"))?;
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
        Ok(SavedRequest{caller:key.clone(),key:NativeKey{store:id(row.get(0)?)?,epoch:positive(row.get(1)?)?,nonce:id(row.get(2)?)?,stage:positive(row.get(3)?)?},intent:row.get(4)?,capability:Capability(row.get::<_,Vec<u8>>(5)?.try_into().map_err(|_|rusqlite::Error::InvalidQuery)?)})
    }).optional()?)
}

pub(crate) struct NativeStore {
    db: CheckedConnection,
    store: Id,
}
impl NativeStore {
    pub(crate) fn create_new(path: &Path) -> Answer<Self> {
        let mut db = CheckedConnection::create_new(path)?;
        let store = *uuid::Uuid::new_v4().as_bytes();
        let tx = db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(&format!("PRAGMA application_id={NATIVE_ROLE}; PRAGMA user_version=1;\n\
            CREATE TABLE meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1),store BLOB NOT NULL CHECK(length(store)=16),epoch INTEGER NOT NULL CHECK(epoch>0)) STRICT;\n\
            CREATE TABLE scope_heads(scope BLOB PRIMARY KEY CHECK(length(scope)=16),generation INTEGER NOT NULL CHECK(generation>=0),owner BLOB NOT NULL CHECK(length(owner)=16)) STRICT;\n\
            CREATE TABLE stages(epoch INTEGER NOT NULL,nonce BLOB NOT NULL CHECK(length(nonce)=16),stage INTEGER NOT NULL,\n\
            intent BLOB NOT NULL CHECK(length(intent)<=65536),cap_hash BLOB NOT NULL CHECK(length(cap_hash)=32),\n\
            scope BLOB,generation INTEGER,owner BLOB,gate INTEGER NOT NULL DEFAULT 0 CHECK(gate IN(0,1,2)),\n\
            spawn INTEGER CHECK(spawn IN(1,2)),stop_requested INTEGER NOT NULL DEFAULT 0 CHECK(stop_requested IN(0,1)),\n\
            resource_report INTEGER NOT NULL DEFAULT 0 CHECK(resource_report IN(0,1,2,3)),\n\
            result_kind INTEGER NOT NULL DEFAULT 0 CHECK(result_kind IN(0,1,2,3)),result_id BLOB,result_hash BLOB,result_body BLOB CHECK(length(result_body)<=65536),\n\
            PRIMARY KEY(epoch,nonce,stage),UNIQUE(scope,generation),\n\
            CHECK((scope IS NULL AND generation IS NULL AND owner IS NULL) OR (scope IS NOT NULL AND generation IS NOT NULL AND owner IS NOT NULL AND length(scope)=16 AND generation>=0 AND length(owner)=16))) STRICT;"))?;
        tx.execute("INSERT INTO meta VALUES(1,?1,1)", [store.as_slice()])?;
        tx.commit()?;
        Ok(Self { db, store })
    }
    pub(crate) fn open(path: &Path) -> Answer<Self> {
        let db = CheckedConnection::open_existing(path, native_schema_valid)?;
        let store = id(db
            .connection()
            .query_row("SELECT store FROM meta WHERE singleton=1", [], |r| r.get(0))?)?;
        Ok(Self { db, store })
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
    pub(crate) fn get(&self, key: NativeKey) -> Answer<Option<Snapshot>> {
        self.checked_key(key)?;
        read_stage(self.db.connection(), key)
    }
    pub(crate) fn create(&mut self, saved: &SavedRequest) -> Answer<Snapshot> {
        self.checked_key(saved.key)?;
        bounded(&saved.intent)?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(previous) = read_stage(&tx, saved.key)? {
            if previous.intent != saved.intent {
                return reject(Reject::IntentChanged);
            }
            authenticate(&tx, saved.key, &saved.capability)?;
            return Ok(previous);
        }
        let epoch: u64 = positive(tx.query_row("SELECT epoch FROM meta WHERE singleton=1", [], |r| r.get(0))?)?;
        if saved.key.epoch < epoch {
            return reject(Reject::ClosedEpoch);
        }
        if saved.key.epoch > epoch {
            return reject(Reject::FutureEpoch);
        }
        tx.execute(
            "INSERT INTO stages(epoch,nonce,stage,intent,cap_hash) VALUES(?1,?2,?3,?4,?5)",
            params![
                number(saved.key.epoch)?,
                saved.key.nonce.as_slice(),
                number(saved.key.stage)?,
                saved.intent,
                digest(&saved.capability.0)
            ],
        )?;
        let snapshot = read_stage(&tx, saved.key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
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
        authenticate(&tx, key, capability)?;
        let old = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if let Some(previous) = old.scope {
            return if previous == scope {
                Ok(Binding { key, scope })
            } else {
                reject(Reject::ScopeAlreadyBound)
            };
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
        tx.commit()?;
        Ok(Binding { key, scope })
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
        let current: (i64, Vec<u8>) = tx.query_row(
            "SELECT generation,owner FROM scope_heads WHERE scope=?1",
            [binding.scope.id.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if current != (number(binding.scope.generation)?, binding.scope.owner.to_vec()) {
            return reject(Reject::StaleGeneration);
        }
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
        Ok(Consume::Granted(LaunchPermit { binding }))
    }
    pub(crate) fn cancel(&mut self, binding: Binding, capability: &Capability) -> Answer<()> {
        self.mutate_bound(binding, capability, |tx, old| {
            let gate = if old.gate == Gate::Pending {
                2
            } else {
                gate_code(old.gate)
            };
            tx.execute(
                "UPDATE stages SET gate=?1,stop_requested=1 WHERE epoch=?2 AND nonce=?3 AND stage=?4",
                params![
                    gate,
                    number(binding.key.epoch)?,
                    binding.key.nonce.as_slice(),
                    number(binding.key.stage)?
                ],
            )?;
            Ok(())
        })
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
                "UPDATE stages SET resource_report=?1,gate=?2 WHERE epoch=?3 AND nonce=?4 AND stage=?5",
                params![
                    report_code(report),
                    gate,
                    number(binding.key.epoch)?,
                    binding.key.nonce.as_slice(),
                    number(binding.key.stage)?
                ],
            )?;
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
        self.mutate_bound(binding,capability,|tx,old|{
            let Some((previous,_))=old.result.identity() else{return reject(Reject::ResultUnknown)};
            if previous!=result {return reject(Reject::ConflictingResult)}
            let kind=if expire || matches!(old.result,ResultKnowledge::Expired{..}){3}else{2};
            tx.execute("UPDATE stages SET result_kind=?1,result_body=CASE WHEN ?1=3 THEN NULL ELSE result_body END WHERE epoch=?2 AND nonce=?3 AND stage=?4",params![kind,number(binding.key.epoch)?,binding.key.nonce.as_slice(),number(binding.key.stage)?])?;
            Ok(())
        })
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
    let hash: Option<Vec<u8>> = db
        .query_row(
            "SELECT cap_hash FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce.as_slice(), number(key.stage)?],
            |r| r.get(0),
        )
        .optional()?;
    match hash {
        None => reject(Reject::UnknownStage),
        Some(hash) if hash != digest(&capability.0) => reject(Reject::CapabilityDenied),
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
fn read_stage(db: &Connection, key: NativeKey) -> Answer<Option<Snapshot>> {
    Ok(db.query_row("SELECT intent,scope,generation,owner,gate,spawn,stop_requested,resource_report,result_kind,result_id,result_hash,result_body FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",params![number(key.epoch)?,key.nonce.as_slice(),number(key.stage)?],|r| {
        let scope=if let Some(bytes)=r.get::<_,Option<Vec<u8>>>(1)? {Some(Scope{id:id(bytes)?,generation:positive(r.get(2)?)?,owner:id(r.get(3)?)?})}else{None};
        let gate=match r.get::<_,i64>(4)? {0=>Gate::Pending,1=>Gate::Consumed,2=>Gate::Sealed,_=>return Err(rusqlite::Error::InvalidQuery)};
        let spawn=match r.get::<_,Option<i64>>(5)? {None=>None,Some(1)=>Some(Spawn::Spawned),Some(2)=>Some(Spawn::Failed),_=>return Err(rusqlite::Error::InvalidQuery)};
        let resource_report=match r.get::<_,i64>(7)? {0=>ResourceReport::None,1=>ResourceReport::Open,2=>ResourceReport::Stopping,3=>ResourceReport::Closed,_=>return Err(rusqlite::Error::InvalidQuery)};
        let result=match r.get::<_,i64>(8)? {
            0=>ResultKnowledge::Pending,
            kind @ 1..=3=>{
                let id=id(r.get(9)?)?;let hash=r.get(10)?;
                match kind {1=>ResultKnowledge::Retained{id,hash,body:r.get(11)?},2=>ResultKnowledge::Acknowledged{id,hash,body:r.get(11)?},_=>ResultKnowledge::Expired{id,hash}}
            }
            _=>return Err(rusqlite::Error::InvalidQuery),
        };
        Ok(Snapshot{key,intent:r.get(0)?,scope,gate,spawn,stop_requested:r.get(6)?,resource_report,result})
    }).optional()?)
}
