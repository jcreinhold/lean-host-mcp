//! Checked connections for the disposable store prototype, not a live database.
//!
//! `fullfsync = 1` records SQLite's requested setting. The bundled Unix VFS can
//! fall back to ordinary fsync; this module does not prove physical durability.

use rusqlite::{Connection, OpenFlags};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const EXPECTED_VERSION: &str = "3.54.1";
const EXPECTED_VERSION_NUMBER: i32 = 3_054_001;
const EXPECTED_SOURCE_ID: &str = "2026-10-16 14:45:20 555e07ef33f69a0957d24d8e555b702464a131d6f1607f4c3c1e4551e8e6ca76";
const BUSY_TIMEOUT: Duration = Duration::from_millis(1_000);
const BUSY_TIMEOUT_MILLIS: i64 = 1_000;
const SELECTED_VFS: &str = "unix";

/// Actual linked SQLite facts, captured before opening the store file.
///
/// The selected VFS is supplied explicitly at open, not read through an unsafe
/// VFS pointer. Build/link provenance remains a separate qualification record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeProvenance {
    pub(crate) version: String,
    pub(crate) version_number: i32,
    pub(crate) source_id: String,
    pub(crate) compile_options: Vec<String>,
    pub(crate) selected_vfs: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveProfile {
    pub(crate) journal_mode: String,
    pub(crate) synchronous: i64,
    pub(crate) fullfsync: i64,
    pub(crate) foreign_keys: i64,
    pub(crate) busy_timeout_millis: i64,
}

impl EffectiveProfile {
    fn is_required(&self) -> bool {
        self.journal_mode == "wal"
            && self.synchronous == 2
            && self.fullfsync == 1
            && self.foreign_keys == 1
            && self.busy_timeout_millis == BUSY_TIMEOUT_MILLIS
    }
}

#[derive(Debug)]
pub(crate) enum ConnectionError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    NotAnAbsoluteRegularFile(PathBuf),
    UnsupportedRuntime(RuntimeProvenance),
    UnexpectedProfile(EffectiveProfile),
    WrongSchema,
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite connection check failed: {error}"),
            Self::Io(error) => write!(formatter, "store file check failed: {error}"),
            Self::NotAnAbsoluteRegularFile(path) => {
                write!(
                    formatter,
                    "expected an existing absolute regular store file: {}",
                    path.display()
                )
            }
            Self::UnsupportedRuntime(actual) => write!(
                formatter,
                "unsupported linked SQLite build: {} / {} / {} / {:?}",
                actual.version, actual.version_number, actual.source_id, actual.compile_options
            ),
            Self::WrongSchema => formatter.write_str("database role or schema is not supported"),
            Self::UnexpectedProfile(actual) => {
                write!(
                    formatter,
                    "SQLite connection settings differ from the required profile: {actual:?}"
                )
            }
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::NotAnAbsoluteRegularFile(_)
            | Self::UnsupportedRuntime(_)
            | Self::UnexpectedProfile(_)
            | Self::WrongSchema => None,
        }
    }
}

impl From<rusqlite::Error> for ConnectionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for ConnectionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn validate_runtime(actual: &RuntimeProvenance) -> Result<(), ConnectionError> {
    const FORBIDDEN: [&str; 5] = [
        "NO_SYNC",
        "DISABLE_DIRSYNC",
        "OMIT_WAL",
        "OMIT_FOREIGN_KEY",
        "OMIT_TRIGGER",
    ];
    if actual.version != EXPECTED_VERSION
        || actual.version_number != EXPECTED_VERSION_NUMBER
        || actual.source_id != EXPECTED_SOURCE_ID
        || !actual.compile_options.iter().any(|option| option == "THREADSAFE=1")
        || actual
            .compile_options
            .iter()
            .any(|option| FORBIDDEN.contains(&option.as_str()))
    {
        return Err(ConnectionError::UnsupportedRuntime(actual.clone()));
    }
    Ok(())
}

fn linked_runtime() -> Result<RuntimeProvenance, ConnectionError> {
    // This fresh in-memory connection cannot recover or change the store file.
    // Querying SQL functions obtains the linked library's identity, not headers
    // or another program's Python/sqlite3 runtime.
    let memory = Connection::open_in_memory()?;
    let (version, source_id) = memory.query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    let compile_options = memory
        .prepare("PRAGMA compile_options")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    let actual = RuntimeProvenance {
        version,
        version_number: rusqlite::version_number(),
        source_id,
        compile_options,
        selected_vfs: SELECTED_VFS,
    };
    validate_runtime(&actual)?;
    Ok(actual)
}

/// Owns one checked connection; concrete stores own their format validators.
pub(crate) struct CheckedConnection {
    connection: Connection,
    provenance: RuntimeProvenance,
}

impl CheckedConnection {
    /// Create exclusively. Only this explicit path configures a new empty file.
    pub(crate) fn create_new(path: &Path) -> Result<Self, ConnectionError> {
        let provenance = linked_runtime()?;
        if !path.is_absolute() {
            return Err(ConnectionError::NotAnAbsoluteRegularFile(path.to_owned()));
        }
        // Close the exclusive creation handle before SQLite takes file locks.
        drop(OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?);
        Self::configure(Self::open_file(path)?, provenance)
    }

    /// Validate the concrete format BEFORE requesting persistent profile changes.
    /// The internal callback is reviewed query-only code, not a Rust read-only
    /// capability. SQLite recovery itself can write; this is not a general
    /// guarantee that reading arbitrary damaged databases never changes files.
    pub(crate) fn open_existing(
        path: &Path,
        validate_schema: fn(&Connection) -> rusqlite::Result<bool>,
    ) -> Result<Self, ConnectionError> {
        let provenance = linked_runtime()?;
        let connection = Self::open_file(path)?;
        if !validate_schema(&connection)? {
            return Err(ConnectionError::WrongSchema);
        }
        Self::configure(connection, provenance)
    }

    fn open_file(path: &Path) -> Result<Connection, ConnectionError> {
        if !path.is_absolute() || !fs::symlink_metadata(path)?.is_file() {
            return Err(ConnectionError::NotAnAbsoluteRegularFile(path.to_owned()));
        }
        // No CREATE, URI interpretation, shared cache or implicit default VFS.
        // NOFOLLOW rejects symlinks; this is not protection from hostile parents.
        let flags =
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags_and_vfs(path, flags, SELECTED_VFS)?;
        // Connection-local only; bounds lock waits during schema reads too.
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(connection)
    }

    fn configure(connection: Connection, provenance: RuntimeProvenance) -> Result<Self, ConnectionError> {
        connection.pragma_update(None, "fullfsync", "ON")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(Some("main"), "synchronous", "FULL")?;
        connection.pragma_update(Some("main"), "journal_mode", "WAL")?;
        let checked = Self { connection, provenance };
        checked.check_profile()?;
        Ok(checked)
    }

    pub(crate) const fn provenance(&self) -> &RuntimeProvenance {
        &self.provenance
    }

    /// Read current settings without repairing them. Call before store mutations.
    pub(crate) fn check_profile(&self) -> Result<EffectiveProfile, ConnectionError> {
        let connection = &self.connection;
        let profile = EffectiveProfile {
            journal_mode: connection.pragma_query_value(Some("main"), "journal_mode", |row| row.get(0))?,
            synchronous: connection.pragma_query_value(Some("main"), "synchronous", |row| row.get(0))?,
            fullfsync: connection.pragma_query_value(None, "fullfsync", |row| row.get(0))?,
            foreign_keys: connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?,
            busy_timeout_millis: connection.pragma_query_value(None, "busy_timeout", |row| row.get(0))?,
        };
        if !profile.is_required() {
            return Err(ConnectionError::UnexpectedProfile(profile));
        }
        Ok(profile)
    }

    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) const fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        reason = "isolated connection tests fail at the first missing required fact"
    )]

    use super::*;

    fn existing_file() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("store.sqlite3");
        // Close the creation handle before SQLite acquires any file locks.
        drop(CheckedConnection::create_new(&path).unwrap());
        (directory, path)
    }

    fn empty_test_schema(connection: &Connection) -> rusqlite::Result<bool> {
        let role: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        Ok(role == 0 && version == 0)
    }

    #[test]
    fn existing_and_reopened_connections_use_the_required_profile() {
        let (_directory, path) = existing_file();
        for _ in 0..2 {
            let checked = CheckedConnection::open_existing(&path, empty_test_schema).unwrap();
            assert!(checked.check_profile().unwrap().is_required());
            assert_eq!(checked.provenance().version_number, EXPECTED_VERSION_NUMBER);
            assert_eq!(checked.provenance().source_id, EXPECTED_SOURCE_ID);
            assert_eq!(checked.provenance().selected_vfs, "unix");
        }
    }

    #[test]
    fn missing_store_is_not_created() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().canonicalize().unwrap().join("missing.sqlite3");
        assert!(matches!(
            CheckedConnection::open_existing(&path, empty_test_schema),
            Err(ConnectionError::Io(_))
        ));
        assert!(!path.exists());
    }

    #[test]
    fn profile_checks_detect_changes_without_silently_repairing_them() {
        for (schema, pragma, changed) in [
            (Some("main"), "synchronous", "NORMAL"),
            (None, "fullfsync", "OFF"),
            (None, "foreign_keys", "OFF"),
            (None, "busy_timeout", "0"),
            (Some("main"), "journal_mode", "DELETE"),
        ] {
            let (_directory, path) = existing_file();
            let checked = CheckedConnection::open_existing(&path, empty_test_schema).unwrap();
            checked.connection().pragma_update(schema, pragma, changed).unwrap();
            for _ in 0..2 {
                assert!(
                    matches!(checked.check_profile(), Err(ConnectionError::UnexpectedProfile(_))),
                    "{pragma}"
                );
            }
        }
    }

    #[test]
    fn mismatched_runtime_or_disabled_sync_is_rejected() {
        let actual = linked_runtime().unwrap();
        for changed in [
            RuntimeProvenance {
                version_number: 3_051_001,
                ..actual.clone()
            },
            RuntimeProvenance {
                source_id: "unqualified source".into(),
                ..actual.clone()
            },
            RuntimeProvenance {
                compile_options: vec!["THREADSAFE=1".into(), "NO_SYNC".into()],
                ..actual
            },
        ] {
            assert!(matches!(
                validate_runtime(&changed),
                Err(ConnectionError::UnsupportedRuntime(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn final_symlink_is_not_opened() {
        let (directory, path) = existing_file();
        let alias = directory.path().canonicalize().unwrap().join("alias.sqlite3");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(matches!(
            CheckedConnection::open_existing(&alias, empty_test_schema),
            Err(ConnectionError::NotAnAbsoluteRegularFile(_))
        ));
    }
}
