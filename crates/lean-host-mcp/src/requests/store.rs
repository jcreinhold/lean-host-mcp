//! The request table: one SQLite row for each request a client names.
//!
//! A retry must return the original result and never start a second launch.
//! SQLite owns that work: a row inserted under `BEGIN IMMEDIATE` decides which
//! of two parallel calls with one id runs, and a committed row survives a
//! crash of the server or the machine.
//!
//! A row fixes the request's intent, project root, kind and deadline, then
//! moves from `queued` through `running` to one of the final states `done`,
//! `timed_out` and `interrupted`. A trigger refuses any change to a final row,
//! so a stored result never changes.
//!
//! The database lives at `<store>/requests.sqlite3` in WAL mode with
//! `synchronous=FULL` (and full syncs on macOS). One connection behind a mutex
//! serves the server; async callers reach it through `spawn_blocking`.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension as _, ToSql, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

/// The database file inside the store.
pub const DATABASE_NAME: &str = "requests.sqlite3";

/// The schema version this server writes, kept in SQLite's `user_version`.
const SCHEMA_VERSION: i64 = 1;

/// How long a statement waits for a lock another connection holds.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Version 1 of the schema.
const SCHEMA_V1: &str = "
CREATE TABLE requests(id TEXT PRIMARY KEY, client TEXT, tool TEXT NOT NULL,
  intent TEXT NOT NULL, arguments TEXT NOT NULL, root TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('build','check')),
  state TEXT NOT NULL CHECK(state IN ('queued','running','done','timed_out','interrupted')),
  created_at INTEGER NOT NULL, deadline_at INTEGER NOT NULL, started_at INTEGER,
  finished_at INTEGER, exit_code INTEGER, log_path TEXT, result TEXT) STRICT;
CREATE INDEX requests_state ON requests(state);
CREATE INDEX requests_finished ON requests(finished_at);
CREATE TRIGGER requests_final BEFORE UPDATE ON requests
  WHEN OLD.state IN ('done','timed_out','interrupted')
  BEGIN SELECT RAISE(ABORT,'final row'); END;
";

/// The columns [`read_row`] reads, in its order.
const COLUMNS: &str = "id, client, tool, intent, arguments, root, kind, state, created_at, deadline_at, \
                       started_at, finished_at, exit_code, log_path, result";

/// Where a request stands. `done`, `timed_out` and `interrupted` are final.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Recorded; nothing has started.
    Queued,
    /// Started: the row said `running` before the first step or dispatch.
    Running,
    /// Ended with a stored result, success or failure.
    Done,
    /// Reached its deadline; anything it started has stopped.
    TimedOut,
    /// Ended by a server restart, with no result.
    Interrupted,
}

impl State {
    /// The name the table and the wire use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
        }
    }

    /// The state named `text`, if any.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Queued,
            Self::Running,
            Self::Done,
            Self::TimedOut,
            Self::Interrupted,
        ]
        .into_iter()
        .find(|state| state.as_str() == text)
    }

    /// Whether the request has ended.
    #[must_use]
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Done | Self::TimedOut | Self::Interrupted)
    }
}

/// What a request runs: a build writes the project, a check only reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `lean_build`: Lake steps under the project's write turn.
    Build,
    /// Any other tool: work under the project's read turn.
    Check,
}

impl Kind {
    /// The name the table and the wire use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check => "check",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Build, Self::Check]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

impl ToSql for State {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for State {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text).ok_or_else(|| FromSqlError::Other(format!("unknown request state {text:?}").into()))
    }
}

impl ToSql for Kind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Kind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Self::parse(text).ok_or_else(|| FromSqlError::Other(format!("unknown request kind {text:?}").into()))
    }
}

/// A request to record. Times count milliseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRow {
    /// The client's request id.
    pub id: String,
    /// The client's name and version from `initialize`, when it sent them.
    pub client: Option<String>,
    /// The tool the request runs.
    pub tool: String,
    /// SHA-256 of the canonical `{tool, arguments}` JSON, in hex.
    pub intent: String,
    /// The tool arguments without `request_id`, as JSON.
    pub arguments: String,
    /// The canonical Lake root the request works on.
    pub root: String,
    /// Build or check.
    pub kind: Kind,
    /// When the server recorded the request.
    pub created_at: i64,
    /// When the request must end.
    pub deadline_at: i64,
    /// The build log, for a build.
    pub log_path: Option<String>,
}

impl NewRow {
    fn into_row(self) -> Row {
        Row {
            id: self.id,
            client: self.client,
            tool: self.tool,
            intent: self.intent,
            arguments: self.arguments,
            root: self.root,
            kind: self.kind,
            state: State::Queued,
            created_at: self.created_at,
            deadline_at: self.deadline_at,
            started_at: None,
            finished_at: None,
            exit_code: None,
            log_path: self.log_path,
            result: None,
        }
    }
}

/// A stored request. Times count milliseconds since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The client's request id.
    pub id: String,
    /// The client's name and version from `initialize`, when it sent them.
    pub client: Option<String>,
    /// The tool the request runs.
    pub tool: String,
    /// SHA-256 of the canonical `{tool, arguments}` JSON, in hex.
    pub intent: String,
    /// The tool arguments without `request_id`, as JSON.
    pub arguments: String,
    /// The canonical Lake root the request works on.
    pub root: String,
    /// Build or check.
    pub kind: Kind,
    /// Where the request stands.
    pub state: State,
    /// When the server recorded the request.
    pub created_at: i64,
    /// When the request must end.
    pub deadline_at: i64,
    /// When the row became `running`.
    pub started_at: Option<i64>,
    /// When the row reached a final state.
    pub finished_at: Option<i64>,
    /// The exit code of a build's failed or last step.
    pub exit_code: Option<i64>,
    /// The build log, for a build.
    pub log_path: Option<String>,
    /// The stored `CallToolResult` JSON, when the request ended with one.
    pub result: Option<String>,
}

/// How a request ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    /// A final state.
    pub state: State,
    /// When it ended.
    pub finished_at: i64,
    /// The exit code of a build's failed or last step.
    pub exit_code: Option<i64>,
    /// The `CallToolResult` JSON to return for the request.
    pub result: Option<String>,
}

/// What [`Store::insert_or_get`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Insert {
    /// No row had the id; this call made it, so this call runs the request.
    Inserted(Row),
    /// A row with the id and the same intent exists; nothing new runs.
    Existing(Row),
    /// A row with the id exists for another intent.
    Conflict(Row),
}

/// The number of requests that have not ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestCounts {
    /// Rows recorded but not started.
    pub queued: u64,
    /// Rows started but not ended.
    pub running: u64,
}

/// A failure of the request store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite refused an operation.
    #[error("request store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database file could not be created.
    #[error("request store: {0}")]
    Io(#[from] io::Error),
    /// SQLite would not use its write-ahead log.
    #[error("request store: SQLite chose journal mode {0}, not wal")]
    NotWal(String),
    /// A newer server wrote the database.
    #[error("request store: schema version {0} is newer than this server's version {current}", current = SCHEMA_VERSION)]
    Newer(i64),
    /// A caller tried to end a row in a state that is not final.
    #[error("request store: {0:?} is not a final state")]
    NotFinal(State),
    /// The blocking task that ran the statement failed.
    #[error("request store: {0}")]
    Task(String),
}

/// The request table of one store.
#[derive(Clone, Debug)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open or create `<store_dir>/requests.sqlite3` and bring its schema to
    /// this server's version. The database file gets mode 0600.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created or opened, SQLite refuses WAL
    /// mode, or a newer server wrote the schema.
    pub fn open(store_dir: &Path) -> Result<Self, StoreError> {
        let path = store_dir.join(DATABASE_NAME);
        // SQLite gives its WAL and shared-memory files the database's mode,
        // so the database must exist private before SQLite opens it.
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?;
        let mut conn = Connection::open(&path)?;
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record `new` as `queued`, unless a row already holds its id.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn insert_or_get(&self, new: NewRow) -> Result<Insert, StoreError> {
        self.run(move |conn| insert_or_get(conn, new)).await
    }

    /// Move a `queued` row to `running` at `at`. Returns whether the row was
    /// `queued`.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn mark_running(&self, id: String, at: i64) -> Result<bool, StoreError> {
        self.run(move |conn| mark_running(conn, &id, at)).await
    }

    /// [`Self::mark_running`] for a caller on a blocking thread.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub fn mark_running_blocking(&self, id: &str, at: i64) -> Result<bool, StoreError> {
        mark_running(&self.conn.lock(), id, at)
    }

    /// End a `queued` or `running` row. Returns whether the row had not ended.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails, or when `end.state` is not final.
    pub async fn finish(&self, id: String, end: End) -> Result<bool, StoreError> {
        self.run(move |conn| finish(conn, &id, &end)).await
    }

    /// The row with `id`, if any.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn get(&self, id: String) -> Result<Option<Row>, StoreError> {
        self.run(move |conn| get(conn, &id)).await
    }

    /// Every row that has not ended, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn unfinished(&self) -> Result<Vec<Row>, StoreError> {
        self.run(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') ORDER BY created_at, rowid"
            ))?;
            let rows = statement.query_map([], read_row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Delete ended rows that finished before `before`, then the oldest ended
    /// rows until at most `max_rows` remain. It never deletes a row that has
    /// not ended. The deletes commit before it returns the deleted rows' log
    /// paths for the caller to remove.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails; then nothing is deleted.
    pub async fn prune(&self, before: i64, max_rows: u64) -> Result<Vec<PathBuf>, StoreError> {
        self.run(move |conn| prune(conn, before, max_rows)).await
    }

    /// The number of queued and running rows.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn counts(&self) -> Result<RequestCounts, StoreError> {
        self.run(|conn| {
            let mut statement = conn
                .prepare("SELECT state, COUNT(*) FROM requests WHERE state IN ('queued','running') GROUP BY state")?;
            let mut counts = RequestCounts::default();
            for pair in statement.query_map([], |row| Ok((row.get::<_, State>(0)?, row.get::<_, i64>(1)?)))? {
                let (state, n) = pair?;
                let n = u64::try_from(n).unwrap_or(0);
                match state {
                    State::Queued => counts.queued = n,
                    State::Running => counts.running = n,
                    State::Done | State::TimedOut | State::Interrupted => {}
                }
            }
            Ok(counts)
        })
        .await
    }

    /// The log paths of every row.
    ///
    /// # Errors
    ///
    /// Fails when SQLite fails.
    pub async fn log_paths(&self) -> Result<HashSet<PathBuf>, StoreError> {
        self.run(|conn| {
            let mut statement = conn.prepare("SELECT log_path FROM requests WHERE log_path IS NOT NULL")?;
            let paths = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|path| path.map(PathBuf::from))
                .collect::<Result<HashSet<_>, _>>()?;
            Ok(paths)
        })
        .await
    }

    async fn run<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || work(&mut conn.lock()))
            .await
            .map_err(|error| StoreError::Task(error.to_string()))?
    }
}

fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::NotWal(mode));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    // macOS `fsync` does not reach the disk; F_FULLFSYNC does.
    #[cfg(target_os = "macos")]
    {
        conn.pragma_update(None, "fullfsync", true)?;
        conn.pragma_update(None, "checkpoint_fullfsync", true)?;
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match version {
        0 => {
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        SCHEMA_VERSION => {}
        newer => return Err(StoreError::Newer(newer)),
    }
    tx.commit()?;
    Ok(())
}

fn insert_or_get(conn: &mut Connection, new: NewRow) -> Result<Insert, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = match get(&tx, &new.id)? {
        Some(stored) if stored.intent == new.intent => Insert::Existing(stored),
        Some(stored) => Insert::Conflict(stored),
        None => {
            tx.execute(
                "INSERT INTO requests(id, client, tool, intent, arguments, root, kind, state, created_at, \
                 deadline_at, log_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, ?9, ?10)",
                params![
                    new.id,
                    new.client,
                    new.tool,
                    new.intent,
                    new.arguments,
                    new.root,
                    new.kind,
                    new.created_at,
                    new.deadline_at,
                    new.log_path,
                ],
            )?;
            Insert::Inserted(new.into_row())
        }
    };
    tx.commit()?;
    Ok(found)
}

fn mark_running(conn: &Connection, id: &str, at: i64) -> Result<bool, StoreError> {
    let changed = conn.execute(
        "UPDATE requests SET state='running', started_at=?2 WHERE id=?1 AND state='queued'",
        params![id, at],
    )?;
    Ok(changed == 1)
}

fn finish(conn: &Connection, id: &str, end: &End) -> Result<bool, StoreError> {
    if !end.state.is_final() {
        return Err(StoreError::NotFinal(end.state));
    }
    let changed = conn.execute(
        "UPDATE requests SET state=?2, finished_at=?3, exit_code=?4, result=?5 \
         WHERE id=?1 AND state IN ('queued','running')",
        params![id, end.state, end.finished_at, end.exit_code, end.result],
    )?;
    Ok(changed == 1)
}

fn get(conn: &Connection, id: &str) -> Result<Option<Row>, StoreError> {
    Ok(conn
        .query_row(&format!("SELECT {COLUMNS} FROM requests WHERE id=?1"), [id], read_row)
        .optional()?)
}

fn prune(conn: &mut Connection, before: i64, max_rows: u64) -> Result<Vec<PathBuf>, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut logs = deleted_logs(
        &tx,
        "DELETE FROM requests WHERE state IN ('done','timed_out','interrupted') AND finished_at < ?1 \
         RETURNING log_path",
        before,
    )?;
    let total: i64 = tx.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?;
    let excess = total.saturating_sub(i64::try_from(max_rows).unwrap_or(i64::MAX));
    if excess > 0 {
        logs.extend(deleted_logs(
            &tx,
            "DELETE FROM requests WHERE id IN (SELECT id FROM requests \
             WHERE state IN ('done','timed_out','interrupted') ORDER BY finished_at, created_at, id LIMIT ?1) \
             RETURNING log_path",
            excess,
        )?);
    }
    tx.commit()?;
    Ok(logs)
}

/// Run the `DELETE ... RETURNING log_path` statement `sql` with `value` and
/// collect the deleted rows' log paths.
fn deleted_logs(conn: &Connection, sql: &str, value: i64) -> Result<Vec<PathBuf>, StoreError> {
    let mut statement = conn.prepare(sql)?;
    let paths = statement
        .query_map([value], |row| row.get::<_, Option<String>>(0))?
        .filter_map(Result::transpose)
        .map(|path| path.map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(paths)
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: row.get(0)?,
        client: row.get(1)?,
        tool: row.get(2)?,
        intent: row.get(3)?,
        arguments: row.get(4)?,
        root: row.get(5)?,
        kind: row.get(6)?,
        state: row.get(7)?,
        created_at: row.get(8)?,
        deadline_at: row.get(9)?,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
        exit_code: row.get(12)?,
        log_path: row.get(13)?,
        result: row.get(14)?,
    })
}
