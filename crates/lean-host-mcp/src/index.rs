//! Persistent declaration index: one `SQLite` database per Lake project.
//!
//! The index is the only path through which `find_symbol`, `find_lemma`,
//! and `outline` answer their queries. It hides:
//!
//! - the `SQLite` schema (one `declarations` table + a `meta` key/value
//!   sidecar for the freshness fingerprint),
//! - the cache-directory layout (`$XDG_CACHE_HOME/lean-host-mcp/…`),
//! - the rebuild pipeline (filter → list → bulk-describe → insert),
//! - the Lake-manifest fingerprint that decides when a rebuild is due.
//!
//! Nothing past this module's boundary should know about `rusqlite` or
//! `sha2`; if a fourth caller needs a new query, add a method here.
//!
//! The struct is `Send + Sync` so it lives behind an `Arc` on
//! [`crate::tools::ToolContext`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Result, ServerError};
use crate::projections::{DeclarationRow, SourceRange};

/// One row of the declaration index, projected for JSON.
///
/// Mirrors [`DeclarationRow`] today; kept as a separate type because the
/// index can synthesise rows the session never built (e.g., a stale row
/// surviving a rebuild crash) and may grow index-only fields like
/// `last_indexed_at` without coupling the session layer.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IndexedDeclaration {
    pub name: String,
    pub kind: String,
    pub type_signature: Option<String>,
    pub source: Option<SourceRange>,
}

/// `SQLite`-backed declaration index. Owns one connection guarded by a
/// `Mutex`: `SQLite`'s single-writer model makes short critical sections
/// fine, and one connection avoids re-running `PRAGMA` per query.
//
// Every method holds the `MutexGuard` for the duration of its query—that
// is the intended granularity (queries are short, the lock protects the
// connection across `prepare` + `query_map` + iterator drain). Clippy's
// "tightening" suggestion fragments the API for no benefit.
#[allow(
    clippy::significant_drop_tightening,
    reason = "MutexGuard lifetime matches each query's natural span"
)]
pub struct DeclarationIndex {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for DeclarationIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclarationIndex").finish_non_exhaustive()
    }
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS declarations (
        name           TEXT PRIMARY KEY,
        kind           TEXT NOT NULL,
        type_signature TEXT,
        file           TEXT,
        start_line     INTEGER,
        start_column   INTEGER,
        end_line       INTEGER,
        end_column     INTEGER
    );
    CREATE INDEX IF NOT EXISTS decl_kind       ON declarations(kind);
    CREATE INDEX IF NOT EXISTS decl_name_lower ON declarations(LOWER(name));
";

const SELECT_COLUMNS: &str = "name, kind, type_signature, file, start_line, start_column, end_line, end_column";

#[allow(
    clippy::significant_drop_tightening,
    reason = "MutexGuard lifetime matches each query's natural span"
)]
impl DeclarationIndex {
    /// Open (or create) the `SQLite` database for `lake_root` under
    /// `cache_dir`. The filename is derived from a short hash of
    /// `lake_root` so two projects on the same machine don't collide.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] if the cache directory cannot be
    /// created, the database cannot be opened, or the schema cannot be
    /// applied.
    pub fn open(cache_dir: &Path, lake_root: &str) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| ServerError::Index(format!("create cache dir {}: {e}", cache_dir.display())))?;
        let db_path = cache_dir.join(format!("decls-{}.sqlite3", short_hash(lake_root)));
        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| ServerError::Index(format!("open sqlite at {}: {e}", db_path.display())))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| ServerError::Index(format!("apply schema: {e}")))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// In-memory variant for tests. Schema is applied; nothing persists
    /// past process exit.
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| ServerError::Index(format!("open in-memory: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| ServerError::Index(format!("apply schema: {e}")))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// True when the stored fingerprint equals `fingerprint`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] if the mutex is poisoned or the
    /// query fails.
    pub fn is_fresh(&self, fingerprint: &str) -> Result<bool> {
        let conn = self.lock()?;
        let stored: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'fingerprint'", [], |row| row.get(0))
            .optional()
            .map_err(|e| ServerError::Index(format!("read fingerprint: {e}")))?;
        Ok(stored.as_deref() == Some(fingerprint))
    }

    /// Case-insensitive substring match on `name`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] on `SQLite` failure.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<IndexedDeclaration>> {
        self.search_impl(query, limit, None)
    }

    /// Case-insensitive substring match on `name`, restricted to
    /// declarations whose `kind` is `"theorem"`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] on `SQLite` failure.
    pub fn search_theorems(&self, query: &str, limit: usize) -> Result<Vec<IndexedDeclaration>> {
        self.search_impl(query, limit, Some("theorem"))
    }

    /// Equality lookup. Returns `None` when the name is absent.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] on `SQLite` failure.
    pub fn lookup(&self, name: &str) -> Result<Option<IndexedDeclaration>> {
        let conn = self.lock()?;
        let sql = format!("SELECT {SELECT_COLUMNS} FROM declarations WHERE name = ?1");
        conn.query_row(&sql, params![name], |row| Ok(row_to_decl(row)))
            .optional()
            .map_err(|e| ServerError::Index(format!("lookup {name}: {e}")))
    }

    /// Name-prefix listing. With `prefix = None`, returns the full table
    /// sorted by name, capped at `limit`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] on `SQLite` failure.
    pub fn outline(&self, prefix: Option<&str>, limit: usize) -> Result<Vec<IndexedDeclaration>> {
        let limit = i64::try_from(limit.min(500)).unwrap_or(i64::MAX);
        let conn = self.lock()?;
        match prefix {
            Some(p) => {
                let pattern = format!("{p}%");
                let sql =
                    format!("SELECT {SELECT_COLUMNS} FROM declarations WHERE name LIKE ?1 ORDER BY name LIMIT ?2");
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| ServerError::Index(format!("prepare outline: {e}")))?;
                let rows = stmt
                    .query_map(params![pattern, limit], |row| Ok(row_to_decl(row)))
                    .map_err(|e| ServerError::Index(format!("query outline: {e}")))?;
                collect_decls(rows)
            }
            None => {
                let sql = format!("SELECT {SELECT_COLUMNS} FROM declarations ORDER BY name LIMIT ?1");
                let mut stmt = conn
                    .prepare(&sql)
                    .map_err(|e| ServerError::Index(format!("prepare outline: {e}")))?;
                let rows = stmt
                    .query_map(params![limit], |row| Ok(row_to_decl(row)))
                    .map_err(|e| ServerError::Index(format!("query outline: {e}")))?;
                collect_decls(rows)
            }
        }
    }

    /// Atomically replace every row and stamp the fingerprint last.
    /// Public for the rebuild pipeline and tests.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Index`] on `SQLite` failure.
    pub fn replace_all(&self, rows: &[DeclarationRow], fingerprint: &str) -> Result<usize> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| ServerError::Index(format!("begin tx: {e}")))?;
        tx.execute("DELETE FROM declarations", [])
            .map_err(|e| ServerError::Index(format!("clear: {e}")))?;
        {
            let sql = format!(
                "INSERT OR REPLACE INTO declarations ({SELECT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            );
            let mut stmt = tx
                .prepare(&sql)
                .map_err(|e| ServerError::Index(format!("prepare insert: {e}")))?;
            for row in rows {
                let (file, sl, sc, el, ec) = match &row.source {
                    Some(s) => (
                        Some(s.file.as_str()),
                        Some(s.start_line),
                        Some(s.start_column),
                        Some(s.end_line),
                        Some(s.end_column),
                    ),
                    None => (None, None, None, None, None),
                };
                stmt.execute(params![row.name, row.kind, row.type_signature, file, sl, sc, el, ec])
                    .map_err(|e| ServerError::Index(format!("insert {}: {e}", row.name)))?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('fingerprint', ?1)",
            params![fingerprint],
        )
        .map_err(|e| ServerError::Index(format!("write fingerprint: {e}")))?;
        tx.commit().map_err(|e| ServerError::Index(format!("commit: {e}")))?;
        Ok(rows.len())
    }

    fn search_impl(&self, query: &str, limit: usize, kind: Option<&str>) -> Result<Vec<IndexedDeclaration>> {
        let limit = i64::try_from(limit.min(500)).unwrap_or(i64::MAX);
        let pattern = format!("%{}%", query.to_lowercase());
        let conn = self.lock()?;
        if let Some(kind) = kind {
            let sql = format!(
                "SELECT {SELECT_COLUMNS} FROM declarations WHERE LOWER(name) LIKE ?1 AND kind = ?2 ORDER BY name LIMIT ?3"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| ServerError::Index(format!("prepare search: {e}")))?;
            let rows = stmt
                .query_map(params![pattern, kind, limit], |row| Ok(row_to_decl(row)))
                .map_err(|e| ServerError::Index(format!("query search: {e}")))?;
            collect_decls(rows)
        } else {
            let sql =
                format!("SELECT {SELECT_COLUMNS} FROM declarations WHERE LOWER(name) LIKE ?1 ORDER BY name LIMIT ?2");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| ServerError::Index(format!("prepare search: {e}")))?;
            let rows = stmt
                .query_map(params![pattern, limit], |row| Ok(row_to_decl(row)))
                .map_err(|e| ServerError::Index(format!("query search: {e}")))?;
            collect_decls(rows)
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| ServerError::Index("connection mutex poisoned".into()))
    }
}

fn row_to_decl(row: &Row<'_>) -> IndexedDeclaration {
    let name: String = row.get(0).unwrap_or_default();
    let kind: String = row.get(1).unwrap_or_default();
    let type_signature: Option<String> = row.get(2).ok().flatten();
    let file: Option<String> = row.get(3).ok().flatten();
    let sl: Option<u32> = row.get(4).ok().flatten();
    let sc: Option<u32> = row.get(5).ok().flatten();
    let el: Option<u32> = row.get(6).ok().flatten();
    let ec: Option<u32> = row.get(7).ok().flatten();
    let source = match (file, sl, sc, el, ec) {
        (Some(file), Some(start_line), Some(start_column), Some(end_line), Some(end_column)) => Some(SourceRange {
            file,
            start_line,
            start_column,
            end_line,
            end_column,
        }),
        _ => None,
    };
    IndexedDeclaration {
        name,
        kind,
        type_signature,
        source,
    }
}

fn collect_decls<I>(rows: I) -> Result<Vec<IndexedDeclaration>>
where
    I: Iterator<Item = rusqlite::Result<IndexedDeclaration>>,
{
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| ServerError::Index(format!("collect rows: {e}")))
}

fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut s = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Default cache directory for the index. Honours `XDG_CACHE_HOME`, then
/// `HOME/.cache`, falling back to `./.lean-host-mcp-cache` if neither is
/// set.
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("lean-host-mcp");
    }
    if let Some(dir) = std::env::var_os("HOME") {
        return PathBuf::from(dir).join(".cache").join("lean-host-mcp");
    }
    PathBuf::from(".lean-host-mcp-cache")
}

/// SHA-256 of `lake-manifest.json` under `lake_root`. The manifest pins
/// every transitive dependency revision, so its hash is a tight upper
/// bound on "is the declaration set still the same".
///
/// # Errors
///
/// Returns [`ServerError::Index`] if the manifest cannot be read.
pub fn fingerprint_lake_project(lake_root: &Path) -> Result<String> {
    let manifest = lake_root.join("lake-manifest.json");
    let bytes =
        std::fs::read(&manifest).map_err(|e| ServerError::Index(format!("read {}: {e}", manifest.display())))?;
    let digest = Sha256::digest(&bytes);
    let mut s = String::with_capacity(digest.len().saturating_mul(2));
    for byte in &digest {
        let _ = write!(s, "{byte:02x}");
    }
    Ok(s)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "test code uses unwrap/index to surface failure paths concisely"
)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, ty: Option<&str>) -> DeclarationRow {
        DeclarationRow {
            name: name.to_owned(),
            kind: kind.to_owned(),
            type_signature: ty.map(str::to_owned),
            source: None,
        }
    }

    #[test]
    fn search_is_case_insensitive() {
        let idx = DeclarationIndex::open_in_memory().unwrap();
        idx.replace_all(
            &[
                row("Nat.add_zero", "theorem", Some("∀ n, n + 0 = n")),
                row("Nat.add", "definition", Some("Nat → Nat → Nat")),
                row("List.map", "definition", None),
            ],
            "fp1",
        )
        .unwrap();

        let hits = idx.search("ADD", 100).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|d| d.name == "Nat.add_zero"));
        assert!(hits.iter().any(|d| d.name == "Nat.add"));
    }

    #[test]
    fn search_theorems_filters_kind() {
        let idx = DeclarationIndex::open_in_memory().unwrap();
        idx.replace_all(
            &[row("Nat.add_zero", "theorem", None), row("Nat.add", "definition", None)],
            "fp1",
        )
        .unwrap();

        let hits = idx.search_theorems("add", 100).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Nat.add_zero");
    }

    #[test]
    fn outline_prefix_matches_and_orders() {
        let idx = DeclarationIndex::open_in_memory().unwrap();
        idx.replace_all(
            &[
                row("Nat.zero", "definition", None),
                row("Nat.succ", "constructor", None),
                row("List.nil", "constructor", None),
            ],
            "fp1",
        )
        .unwrap();

        let hits = idx.outline(Some("Nat."), 100).unwrap();
        assert_eq!(
            hits.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["Nat.succ", "Nat.zero"]
        );

        let all = idx.outline(None, 100).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn lookup_returns_some_then_none() {
        let idx = DeclarationIndex::open_in_memory().unwrap();
        idx.replace_all(&[row("Nat.zero", "definition", Some("Nat"))], "fp1")
            .unwrap();

        let hit = idx.lookup("Nat.zero").unwrap();
        assert_eq!(hit.as_ref().map(|d| d.name.as_str()), Some("Nat.zero"));
        assert_eq!(hit.unwrap().type_signature.as_deref(), Some("Nat"));

        assert!(idx.lookup("Nat.bogus").unwrap().is_none());
    }

    #[test]
    fn fingerprint_round_trip() {
        let idx = DeclarationIndex::open_in_memory().unwrap();
        assert!(!idx.is_fresh("fp1").unwrap());
        idx.replace_all(&[], "fp1").unwrap();
        assert!(idx.is_fresh("fp1").unwrap());
        assert!(!idx.is_fresh("fp2").unwrap());
        idx.replace_all(&[], "fp2").unwrap();
        assert!(!idx.is_fresh("fp1").unwrap());
        assert!(idx.is_fresh("fp2").unwrap());
    }
}
