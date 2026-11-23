//! Private one-stage recovery lookup. The registry remains the native authority.
use super::{
    AdmissionDeadline, Answer, Binding, CheckedConnection, Gate, Id, NativeKey, NativeStore, OptionalExtension, Path,
    Problem, Reject, ResourceReport, SavedRequest, Scope, Transaction, TransactionBehavior, authenticate,
    bind_scope_transaction, blob, bound_stage, bounded, commit, digest, fixed, native_schema_valid, number,
    optional_fixed, params, positive, read_stage, read_stage_columns, reject,
};
use lean_toolchain::native_guardian::NativeProcessIdentity;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

pub(super) const SCHEMA: &str = "
    CREATE TABLE recovery_bindings(
        epoch INTEGER NOT NULL, nonce BLOB NOT NULL, stage INTEGER NOT NULL,
        job BLOB NOT NULL CHECK(length(job) BETWEEN 1 AND 65536),
        caller_stage INTEGER NOT NULL, revision INTEGER NOT NULL,
        registry_generation TEXT NOT NULL UNIQUE CHECK(
            length(CAST(registry_generation AS BLOB))=48 AND registry_generation NOT GLOB '*[^0-9a-f]*'
        ),
        identity BLOB NOT NULL CHECK(length(identity) BETWEEN 1 AND 32768),
        expires_ns INTEGER CHECK(expires_ns > 0),
        PRIMARY KEY(epoch,nonce,stage), UNIQUE(job,caller_stage,revision),
        FOREIGN KEY(epoch,nonce,stage) REFERENCES stages(epoch,nonce,stage)
    ) STRICT;";
// A warm store must refuse older binaries: their recovery lookup has only
// generation identity and would let a historical attempt drain its successor.
pub(super) const WARM_SCHEMA_VERSION: i64 = 6;

pub(super) fn warm_schema(db: &rusqlite::Connection) -> rusqlite::Result<bool> {
    db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map(|version| version == WARM_SCHEMA_VERSION)
}

#[cfg(test)]
const PROMOTE_WARM: &str = "
    ALTER TABLE recovery_bindings RENAME TO prior_recovery_bindings;
    CREATE TABLE recovery_bindings(
        epoch INTEGER NOT NULL, nonce BLOB NOT NULL, stage INTEGER NOT NULL,
        job BLOB NOT NULL CHECK(length(job) BETWEEN 1 AND 65536),
        caller_stage INTEGER NOT NULL, revision INTEGER NOT NULL,
        registry_generation TEXT NOT NULL CHECK(
            length(CAST(registry_generation AS BLOB))=48 AND registry_generation NOT GLOB '*[^0-9a-f]*'
        ),
        identity BLOB NOT NULL CHECK(length(identity) BETWEEN 1 AND 32768),
        expires_ns INTEGER CHECK(expires_ns > 0),
        ordinal INTEGER CHECK(ordinal > 0),
        ended INTEGER NOT NULL DEFAULT 0 CHECK(ended IN (0,1) AND (ended=0 OR ordinal IS NOT NULL)),
        PRIMARY KEY(epoch,nonce,stage), UNIQUE(job,caller_stage,revision),
        UNIQUE(registry_generation,ordinal),
        FOREIGN KEY(epoch,nonce,stage) REFERENCES stages(epoch,nonce,stage)
    ) STRICT;
    INSERT INTO recovery_bindings
        SELECT epoch,nonce,stage,job,caller_stage,revision,registry_generation,identity,expires_ns,NULL,0
        FROM prior_recovery_bindings;
    DROP TABLE prior_recovery_bindings;
    CREATE UNIQUE INDEX recovery_unfinished ON recovery_bindings(registry_generation) WHERE ended=0;";

const IDENTITY_LIMIT: usize = 32_768;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryIdentity {
    pub(crate) directory: Vec<u8>,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) generation: String,
    pub(crate) boot: Id,
    pub(crate) parent: NativeProcessIdentity,
    pub(crate) worker: NativeProcessIdentity,
    pub(crate) guardian: NativeProcessIdentity,
}

fn valid_generation(generation: &str) -> bool {
    generation.len() == 48
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl RegistryIdentity {
    fn valid(&self) -> bool {
        let directory = Path::new(std::ffi::OsStr::from_bytes(&self.directory));
        directory.is_absolute()
            && self.directory.len() <= 4096
            && !self.directory.contains(&0)
            && self.inode != 0
            && valid_generation(&self.generation)
            && self.boot != [0; 16]
            && self.parent.pid != self.worker.pid
            && self.parent.pid != self.guardian.pid
            && self.worker.pid != self.guardian.pid
            && [&self.parent, &self.worker, &self.guardian]
                .into_iter()
                .all(|identity| {
                    identity.version == 1
                        && !identity.start.is_empty()
                        && identity.start.len() <= 4096
                        && [identity.pid, identity.session, identity.process_group]
                            .into_iter()
                            .all(|id| id != 0 && i32::try_from(id).is_ok())
                })
    }

    pub(crate) fn matches_directory(&self) -> bool {
        let directory = Path::new(std::ffi::OsStr::from_bytes(&self.directory));
        std::fs::symlink_metadata(directory).is_ok_and(|metadata| {
            metadata.is_dir()
                && metadata.uid() == rustix::process::geteuid().as_raw()
                && metadata.mode() & 0o777 == 0o700
                && (metadata.dev(), metadata.ino()) == (self.device, self.inode)
                && directory.canonicalize().is_ok_and(|path| path == directory)
        })
    }
}

/// The exact guardian scope and absolute tick admitted before consume. This is
/// internal launch data, not a caller-supplied duration or recoverable permit.
#[derive(Debug)]
pub(crate) struct DeadlineLaunchGrant {
    binding: Binding,
    registry: RegistryIdentity,
    expires_ns: u64,
    intent: Vec<u8>,
}

impl DeadlineLaunchGrant {
    pub(crate) fn into_parts(self) -> (Binding, RegistryIdentity, u64, Vec<u8>) {
        (self.binding, self.registry, self.expires_ns, self.intent)
    }
}

pub(super) fn boot_id() -> Answer<Id> {
    static BOOT: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    if let Some(boot) = BOOT.get() {
        return Ok(*boot);
    }
    let unavailable = || Problem::Rejected(Reject::InvalidLaunchDeadline);
    #[cfg(target_os = "macos")]
    let bytes = {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .map_err(|_| unavailable())?;
        if !output.status.success() {
            return Err(unavailable());
        }
        output.stdout
    };
    #[cfg(target_os = "linux")]
    let bytes = std::fs::read("/proc/sys/kernel/random/boot_id").map_err(|_| unavailable())?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let bytes: Vec<u8> = Vec::new();
    let boot = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| uuid::Uuid::parse_str(value.trim()).ok())
        .filter(|boot| !boot.is_nil())
        .ok_or_else(unavailable)?;
    Ok(*BOOT.get_or_init(|| *boot.as_bytes()))
}

fn boot_ticks() -> Answer<std::time::Duration> {
    lean_toolchain::native_guardian::guardian_deadline_ticks()
        .map_err(|_| Problem::Rejected(Reject::InvalidLaunchDeadline))
}

pub(crate) fn admission_deadline(budget: std::time::Duration) -> Answer<AdmissionDeadline> {
    let expires_ns = boot_ticks()?
        .checked_add(budget)
        .and_then(|expiry| u64::try_from(expiry.as_nanos()).ok())
        .filter(|&ticks| ticks > 0 && i64::try_from(ticks).is_ok())
        .ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))?;
    Ok(AdmissionDeadline {
        boot: boot_id()?,
        expires_ns,
    })
}

impl AdmissionDeadline {
    pub(crate) fn remaining(self) -> Answer<std::time::Duration> {
        if boot_id()? != self.boot {
            return reject(Reject::InvalidLaunchDeadline);
        }
        std::time::Duration::from_nanos(self.expires_ns)
            .checked_sub(boot_ticks()?)
            .filter(|time| !time.is_zero())
            .ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))
    }

    fn matches(self, identity: &RegistryIdentity, expiry: Option<u64>) -> Answer<()> {
        if identity.boot != self.boot || expiry != Some(self.expires_ns) {
            return reject(Reject::InvalidLaunchDeadline);
        }
        self.remaining().map(|_| ())
    }
}

pub(super) fn check_admitted_grant(admitted: AdmissionDeadline, grant: Option<&DeadlineLaunchGrant>) -> Answer<()> {
    let grant = grant.ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))?;
    admitted.matches(&grant.registry, Some(grant.expires_ns))
}

pub(super) fn launch_deadline(
    tx: &Transaction<'_>,
    binding: Binding,
    intent: Vec<u8>,
) -> Answer<Option<DeadlineLaunchGrant>> {
    let key = binding.key;
    let row = tx
        .query_row(
            "SELECT registry_generation,identity,expires_ns FROM recovery_bindings
             WHERE epoch=?1 AND nonce=?2 AND stage=?3 AND expires_ns IS NOT NULL",
            params![number(key.epoch)?, key.nonce, number(key.stage)?],
            |row| {
                let bytes = blob(row, 1)?;
                if bytes.len() > IDENTITY_LIMIT {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok((row.get::<_, String>(0)?, bytes.to_vec(), positive(row.get(2)?)?))
            },
        )
        .optional()?;
    let Some((generation, encoded, expires_ns)) = row else {
        return Ok(None);
    };
    let registry: RegistryIdentity =
        serde_json::from_slice(&encoded).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
    if !registry.valid() || registry.generation != generation || !registry.matches_directory() {
        return reject(Reject::WrongScope);
    }
    if expires_ns == 0 {
        return reject(Reject::InvalidLaunchDeadline);
    }
    Ok(Some(DeadlineLaunchGrant {
        binding,
        registry,
        expires_ns,
        intent,
    }))
}

impl NativeStore {
    /// The trusted adapter supplies SDK observations under its publication
    /// lock. Seal the exact stage and registry reference in one transaction.
    pub(crate) fn bind_recovery_scope(
        &mut self,
        saved: &SavedRequest,
        scope: Scope,
        identity: &RegistryIdentity,
    ) -> Answer<Binding> {
        self.bind_exact_scope(saved, scope, identity, None, None)
    }

    /// Admit an absolute nanosecond tick from the trusted guardian boot clock.
    /// Bind it before consume. Neither retries nor caller timing can change it.
    pub(crate) fn bind_timed_recovery_scope(
        &mut self,
        saved: &SavedRequest,
        scope: Scope,
        identity: &RegistryIdentity,
        expires_ns: u64,
    ) -> Answer<Binding> {
        let expires_ns = i64::try_from(expires_ns)
            .ok()
            .filter(|tick| *tick > 0)
            .ok_or(Problem::Rejected(Reject::InvalidLaunchDeadline))?;
        self.bind_exact_scope(saved, scope, identity, Some(expires_ns), None)
    }

    fn bind_exact_scope(
        &mut self,
        saved: &SavedRequest,
        scope: Scope,
        identity: &RegistryIdentity,
        expires_ns: Option<i64>,
        ordinal: Option<i64>,
    ) -> Answer<Binding> {
        self.checked_key(saved.key)?;
        bounded(&saved.caller.job)?;
        bounded(&saved.intent)?;
        if saved.caller.job.is_empty() || !identity.valid() || !identity.matches_directory() {
            return reject(Reject::WrongScope);
        }
        let encoded = serde_json::to_vec(identity).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        if encoded.len() > IDENTITY_LIMIT {
            return reject(Reject::Oversized);
        }
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = saved.key;
        authenticate(&tx, key, &saved.capability)?;
        let old = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if old.intent != saved.intent {
            return reject(Reject::IntentChanged);
        }
        if old.scope.is_some() || old.gate != Gate::Pending {
            return reject(Reject::ScopeAlreadyBound);
        }
        if let Some(admitted) = old.admission_deadline {
            admitted.matches(identity, expires_ns.and_then(|ticks| u64::try_from(ticks).ok()))?;
        }
        let warm = warm_schema(&tx)?;
        if warm {
            let prior = tx
                .query_row(
                    "SELECT ordinal,ended,identity=?2,s.resource_report FROM recovery_bindings r
                 JOIN stages s USING(epoch,nonce,stage) WHERE registry_generation=?1 ORDER BY ordinal DESC LIMIT 1",
                    params![identity.generation, encoded],
                    |row| {
                        Ok((
                            row.get::<_, Option<i64>>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            match (ordinal, prior) {
                (None | Some(1), None) => {}
                (Some(next), Some((Some(previous), true, true, 1))) if previous.checked_add(1) == Some(next) => {}
                _ => return reject(Reject::ScopeAlreadyBound),
            }
        } else if ordinal.is_some() {
            return reject(Reject::WrongSchema);
        }
        let binding = bind_scope_transaction(&tx, key, &saved.capability, scope)?;
        tx.execute(
            "INSERT INTO recovery_bindings(epoch,nonce,stage,job,caller_stage,revision,registry_generation,identity,expires_ns)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                number(key.epoch)?, key.nonce, number(key.stage)?, saved.caller.job,
                number(saved.caller.stage)?, number(saved.caller.revision)?, identity.generation, encoded, expires_ns
            ],
        )?;
        if let Some(ordinal) = ordinal {
            tx.execute(
                "UPDATE recovery_bindings SET ordinal=?4 WHERE epoch=?1 AND nonce=?2 AND stage=?3",
                params![number(key.epoch)?, key.nonce, number(key.stage)?, ordinal],
            )?;
        }
        commit(tx, "bind-recovery-scope")?;
        Ok(binding)
    }

    pub(crate) fn authorize_drain(&self, key: NativeKey, secret: Option<[u8; 32]>) -> Answer<()> {
        self.checked_key(key)?;
        let Some(secret) = secret else {
            return reject(Reject::CapabilityDenied);
        };
        let expected = self.db.connection().query_row(
            "SELECT drain_auth_hash FROM stages WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce, number(key.stage)?],
            |row| optional_fixed::<32>(row, 0),
        )?;
        if expected.as_ref().map(<[u8; 32]>::as_slice) != Some(digest(&secret).as_slice()) {
            return reject(Reject::CapabilityDenied);
        }
        Ok(())
    }

    /// Win or recover one exact durable claim. This never returns a launch
    /// grant or changes custody; the original guardian still decides cleanup.
    pub(crate) fn claim_recovery(
        &mut self,
        saved: &SavedRequest,
        binding: Binding,
        identity: &RegistryIdentity,
        authorization: Option<[u8; 32]>,
        claimant: Option<[u8; 32]>,
    ) -> Answer<bool> {
        self.checked_key(saved.key)?;
        let (Some(authorization), Some(claimant)) = (authorization, claimant) else {
            return reject(Reject::CapabilityDenied);
        };
        if claimant == [0; 32] || claimant == authorization || claimant == saved.capability.0 {
            return reject(Reject::CapabilityDenied);
        }
        if binding.key != saved.key || !identity.valid() || !identity.matches_directory() {
            return reject(Reject::WrongScope);
        }
        let encoded = serde_json::to_vec(identity).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = bound_stage(&tx, binding, &saved.capability)?;
        if old.ended_ordinal.is_some() {
            return reject(Reject::WrongScope);
        }
        if !old.matches_intent(&saved.intent) {
            return reject(Reject::IntentChanged);
        }
        let key = saved.key;
        let row = tx
            .query_row(
                "SELECT s.drain_auth_hash,s.recovery_claim_hash
             FROM stages s JOIN recovery_bindings r USING(epoch,nonce,stage)
             JOIN scope_heads h ON h.scope=s.scope AND h.generation=s.generation AND h.owner=s.owner
             WHERE s.epoch=?1 AND s.nonce=?2 AND s.stage=?3
               AND r.job=?4 AND r.caller_stage=?5 AND r.revision=?6
               AND r.registry_generation=?7 AND r.identity=?8",
                params![
                    number(key.epoch)?,
                    key.nonce,
                    number(key.stage)?,
                    saved.caller.job,
                    number(saved.caller.stage)?,
                    number(saved.caller.revision)?,
                    identity.generation,
                    encoded
                ],
                |r| Ok((optional_fixed::<32>(r, 0)?, optional_fixed::<32>(r, 1)?)),
            )
            .optional()?
            .ok_or(Problem::Rejected(Reject::CallerBindingChanged))?;
        if row.0.as_ref().map(<[u8; 32]>::as_slice) != Some(digest(&authorization).as_slice()) {
            return reject(Reject::CapabilityDenied);
        }
        let hash = digest(&claimant);
        if let Some(winner) = row.1 {
            if winner.as_slice() != hash {
                return reject(Reject::RecoveryClaimBusy);
            }
            return Ok(false);
        }
        if old.resource_report == ResourceReport::Closed {
            return reject(Reject::WrongScope);
        }
        tx.execute(
            "UPDATE stages SET recovery_claim_hash=?4,gate=CASE WHEN gate=0 THEN 2 ELSE gate END
             WHERE epoch=?1 AND nonce=?2 AND stage=?3",
            params![number(key.epoch)?, key.nonce, number(key.stage)?, hash],
        )?;
        commit(tx, "recovery-claim")?;
        Ok(true)
    }

    /// Read one authenticated binding without opening the service ownership
    /// lock. The SDK must recheck all of it under its publication lock
    /// before acting on a scope. This read confers no process authority.
    pub(crate) fn read_recovery_scope(
        path: &Path,
        saved: &SavedRequest,
        expected_generation: Option<&str>,
        expected_parent: Option<&NativeProcessIdentity>,
    ) -> Answer<(Binding, RegistryIdentity)> {
        bounded(&saved.caller.job)?;
        bounded(&saved.intent)?;
        if expected_generation.is_some_and(|generation| !valid_generation(generation)) {
            return reject(Reject::WrongScope);
        }
        let mut db = CheckedConnection::open_recovery(path)?;
        let tx = db.transaction()?;
        if !native_schema_valid(&tx)? {
            return reject(Reject::WrongSchema);
        }
        let mode: String = tx.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if mode != "wal" {
            return reject(Reject::WrongSchema);
        }
        let key = saved.key;
        let store: Id = tx.query_row("SELECT store FROM meta WHERE singleton=1", [], |row| fixed(row, 0))?;
        if key.store != store {
            return reject(Reject::WrongStore);
        }
        authenticate(&tx, key, &saved.capability)?;
        // A recovery-only connection can precede a writable schema migration.
        // Closure for a consumed scope is independent of its admission budget.
        let old = read_stage_columns(&tx, key, false)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if old.intent != saved.intent {
            return reject(Reject::IntentChanged);
        }
        let scope = old.scope.ok_or(Problem::Rejected(Reject::WrongScope))?;
        if old.resource_report == ResourceReport::Closed || old.ended_ordinal.is_some() {
            return reject(Reject::WrongScope);
        }
        let head: (i64, Id) = tx.query_row(
            "SELECT generation,owner FROM scope_heads WHERE scope=?1",
            [scope.id],
            |row| Ok((row.get(0)?, fixed(row, 1)?)),
        )?;
        if head != (number(scope.generation)?, scope.owner) {
            return reject(Reject::StaleGeneration);
        }
        let (generation, encoded) = tx
            .query_row(
                "SELECT registry_generation,identity FROM recovery_bindings WHERE epoch=?1 AND nonce=?2 AND stage=?3
                 AND job=?4 AND caller_stage=?5 AND revision=?6",
                params![
                    number(key.epoch)?,
                    key.nonce,
                    number(key.stage)?,
                    saved.caller.job,
                    number(saved.caller.stage)?,
                    number(saved.caller.revision)?
                ],
                |row| {
                    let bytes = row.get_ref(1)?.as_blob()?;
                    if bytes.len() > IDENTITY_LIMIT {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let generation = row.get_ref(0)?.as_str()?;
                    if !valid_generation(generation) {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    Ok((generation.to_owned(), bytes.to_vec()))
                },
            )
            .optional()?
            .ok_or(Problem::Rejected(Reject::CallerBindingChanged))?;
        if expected_generation.is_some_and(|expected| generation != expected) {
            return reject(Reject::StaleGeneration);
        }
        let identity: RegistryIdentity =
            serde_json::from_slice(&encoded).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        if !identity.valid()
            || identity.generation != generation
            || expected_parent.is_some_and(|parent| parent != &identity.parent)
        {
            return reject(Reject::WrongScope);
        }
        tx.commit()?;
        Ok((Binding { key, scope }, identity))
    }
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
impl NativeStore {
    pub(crate) fn open_stage_recovery(
        &self,
        saved: &SavedRequest,
        generation: Option<&str>,
    ) -> Answer<(Binding, RegistryIdentity)> {
        let path = self
            .db
            .connection()
            .path()
            .ok_or(Problem::Rejected(Reject::WrongStore))?;
        Self::read_recovery_scope(Path::new(path), saved, generation, None)
    }

    /// Record trusted SDK closure without granting launch or inventing a result.
    pub(crate) fn reconcile_closed_scope(&mut self, binding: Binding, identity: &RegistryIdentity) -> Answer<()> {
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let encoded = serde_json::to_vec(identity).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE stages SET resource_report=3,gate=CASE WHEN gate=0 THEN 2 ELSE gate END,
             result_body=CASE WHEN result_kind=2 THEN NULL ELSE result_body END,
             result_kind=CASE WHEN result_kind=2 THEN 3 ELSE result_kind END
             WHERE epoch=?1 AND nonce=?2 AND stage=?3 AND scope=?4 AND generation=?5 AND owner=?6
             AND EXISTS(SELECT 1 FROM scope_heads h WHERE h.scope=?4 AND h.generation=?5 AND h.owner=?6)
             AND EXISTS(SELECT 1 FROM recovery_bindings r WHERE r.epoch=?1 AND r.nonce=?2 AND r.stage=?3
                        AND r.identity=?7 AND r.expires_ns IS NOT NULL)",
            params![
                number(binding.key.epoch)?,
                binding.key.nonce,
                number(binding.key.stage)?,
                binding.scope.id,
                number(binding.scope.generation)?,
                binding.scope.owner,
                encoded
            ],
        )?;
        if changed != 1 {
            return reject(Reject::WrongScope);
        }
        super::compact_closed_acknowledged(&tx, binding.key)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct CheckedWarmFinish {
    pub(crate) binding: Binding,
    pub(crate) registry: RegistryIdentity,
    pub(crate) ordinal: u64,
    pub(crate) expires_ns: u64,
}

#[cfg(test)]
impl NativeStore {
    pub(crate) fn promote_warm_contract(&mut self) -> Answer<()> {
        self.db.check_profile()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(PROMOTE_WARM)?;
        tx.pragma_update(None, "user_version", WARM_SCHEMA_VERSION)?;
        commit(tx, "enable-warm-contract")
    }

    pub(crate) fn bind_warm_contract(
        &mut self,
        saved: &SavedRequest,
        scope: Scope,
        registry: &RegistryIdentity,
        expires_ns: u64,
        ordinal: u64,
    ) -> Answer<Binding> {
        if ordinal == 0 || expires_ns == 0 {
            return reject(Reject::InvalidLaunchDeadline);
        }
        self.bind_exact_scope(
            saved,
            scope,
            registry,
            Some(number(expires_ns)?),
            Some(number(ordinal)?),
        )
    }

    // Trusted producer seam. Tests inject the acknowledgement only after
    // constructing the exact tuple. The pending SDK verifier will own it.
    pub(crate) fn end_warm_contract(
        &mut self,
        finish: &CheckedWarmFinish,
        capability: &super::Capability,
    ) -> Answer<()> {
        let binding = finish.binding;
        self.checked_key(binding.key)?;
        self.db.check_profile()?;
        let encoded = serde_json::to_vec(&finish.registry).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !warm_schema(&tx)? {
            return reject(Reject::WrongSchema);
        }
        let old = bound_stage(&tx, binding, capability)?;
        let key = binding.key;
        if old.gate != Gate::Consumed
            || old.resource_report != ResourceReport::Open
            || !matches!(
                old.result,
                super::ResultKnowledge::Retained { .. } | super::ResultKnowledge::Acknowledged { .. }
            )
        {
            return reject(Reject::WrongScope);
        }
        let changed = tx.execute(
            "UPDATE recovery_bindings SET ended=1 WHERE epoch=?1 AND nonce=?2 AND stage=?3
             AND identity=?4 AND ordinal=?5 AND expires_ns=?6 AND ended=0
             AND EXISTS(SELECT 1 FROM stages s WHERE s.epoch=?1 AND s.nonce=?2 AND s.stage=?3
                        AND s.recovery_claim_hash IS NULL AND s.stop_requested=0)",
            params![
                number(key.epoch)?,
                key.nonce,
                number(key.stage)?,
                encoded,
                number(finish.ordinal)?,
                number(finish.expires_ns)?
            ],
        )?;
        if changed != 1 {
            return reject(Reject::WrongScope);
        }
        commit(tx, "end-warm-attempt")
    }
}
