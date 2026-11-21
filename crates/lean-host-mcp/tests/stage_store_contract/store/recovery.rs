//! Private one-stage recovery lookup. The registry remains the native authority.
use super::*;
use lean_toolchain::native_guardian::NativeProcessIdentity;
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

pub(super) const SCHEMA: &str = "
    CREATE TABLE recovery_bindings(
        epoch INTEGER NOT NULL, nonce BLOB NOT NULL, stage INTEGER NOT NULL,
        job BLOB NOT NULL CHECK(length(job) BETWEEN 1 AND 65536),
        caller_stage INTEGER NOT NULL, revision INTEGER NOT NULL,
        registry_generation BLOB NOT NULL UNIQUE CHECK(length(registry_generation)=16),
        identity BLOB NOT NULL CHECK(length(identity) BETWEEN 1 AND 32768),
        PRIMARY KEY(epoch,nonce,stage), UNIQUE(job,caller_stage,revision),
        FOREIGN KEY(epoch,nonce,stage) REFERENCES stages(epoch,nonce,stage)
    ) STRICT;";
const IDENTITY_LIMIT: usize = 32_768;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryIdentity {
    pub(crate) directory: Vec<u8>,
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) generation: Id,
    pub(crate) boot: Id,
    pub(crate) worker: NativeProcessIdentity,
    pub(crate) guardian: NativeProcessIdentity,
}

impl RegistryIdentity {
    fn valid(&self) -> bool {
        let directory = Path::new(std::ffi::OsStr::from_bytes(&self.directory));
        directory.is_absolute()
            && self.directory.len() <= 4096
            && !self.directory.contains(&0)
            && self.inode != 0
            && self.generation != [0; 16]
            && self.boot != [0; 16]
            && self.worker.pid != self.guardian.pid
            && [&self.worker, &self.guardian].into_iter().all(|identity| {
                identity.version == 1
                    && !identity.start.is_empty()
                    && identity.start.len() <= 4096
                    && [identity.pid, identity.session, identity.process_group]
                        .into_iter()
                        .all(|id| id != 0 && i32::try_from(id).is_ok())
            })
    }

    fn matches_directory(&self) -> bool {
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

impl NativeStore {
    /// The trusted adapter supplies SDK observations under its publication
    /// lock. Seal the exact stage and registry reference in one transaction.
    pub(crate) fn bind_recovery_scope(
        &mut self,
        saved: &SavedRequest,
        scope: Scope,
        identity: &RegistryIdentity,
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
        let binding = bind_scope_transaction(&tx, key, &saved.capability, scope)?;
        tx.execute(
            "INSERT INTO recovery_bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
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
        )?;
        commit(tx, "bind-recovery-scope")?;
        Ok(binding)
    }

    /// Read one authenticated binding without opening the service ownership
    /// lock. The SDK must recheck all of it under its publication lock
    /// before acting on a scope. This read confers no process authority.
    pub(crate) fn read_recovery_scope(
        path: &Path,
        saved: &SavedRequest,
        expected_generation: Option<Id>,
    ) -> Answer<(Binding, RegistryIdentity)> {
        bounded(&saved.caller.job)?;
        bounded(&saved.intent)?;
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
        let old = read_stage(&tx, key)?.ok_or(Problem::Rejected(Reject::UnknownStage))?;
        if old.intent != saved.intent {
            return reject(Reject::IntentChanged);
        }
        let scope = old.scope.ok_or(Problem::Rejected(Reject::WrongScope))?;
        if old.resource_report == ResourceReport::Closed {
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
                    Ok((fixed::<16>(row, 0)?, bytes.to_vec()))
                },
            )
            .optional()?
            .ok_or(Problem::Rejected(Reject::CallerBindingChanged))?;
        if expected_generation.is_some_and(|expected| generation != expected) {
            return reject(Reject::StaleGeneration);
        }
        let identity: RegistryIdentity =
            serde_json::from_slice(&encoded).map_err(|_| Problem::Rejected(Reject::WrongScope))?;
        if !identity.valid() || identity.generation != generation {
            return reject(Reject::WrongScope);
        }
        tx.commit()?;
        Ok((Binding { key, scope }, identity))
    }
}
