//! Tool implementations.
//!
//! Split by what plumbing they share rather than one file per tool:
//!
//! - [`lean`] — `elaborate`, `kernel_check`, `infer_type`, `whnf`,
//!   `is_def_eq`, `hover_by_name`. All six drive a `SessionHost` and
//!   project Lean responses into the JSON envelope.
//! - [`scan`] — `project_scan`. No Lean dependency; pure filesystem walk
//!   with a configurable regex.
//! - [`index`] — `find_symbol`, `find_lemma`, `outline`. Thin wrappers
//!   over the SQLite-backed [`DeclarationIndex`](crate::DeclarationIndex);
//!   rebuild on Lake-manifest change.
//! - [`position`] — `goal_at_position`, `type_at_position`,
//!   `references_of_name`. Thin lookups over a `ProcessedFileCache`-backed
//!   [`ProcessedFile`](lean_rs_host::host::process::ProcessedFile)
//!   projection; the cache is keyed on path + content hash.

use std::sync::Arc;

pub mod index;
pub mod lean;
pub mod position;
pub mod scan;

use crate::cache::ProcessedFileCache;
use crate::envelope::Freshness;
use crate::index::DeclarationIndex;
use crate::session::SessionHost;

/// Shared state every tool handler reads.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub host: SessionHost,
    pub index: Arc<DeclarationIndex>,
    pub processed_files: Arc<ProcessedFileCache>,
    pub lake_root: String,
    pub default_imports: Vec<String>,
}

impl ToolContext {
    pub fn freshness(&self, imports: &[String], session_id: &str) -> Freshness {
        let imports = if imports.is_empty() {
            self.default_imports.clone()
        } else {
            imports.to_vec()
        };
        Freshness {
            lake_root: self.lake_root.clone(),
            imports,
            session_id: session_id.to_owned(),
            lean_toolchain: self.host.lean_toolchain().to_owned(),
        }
    }
}

pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Directory names skipped during `.lean` file enumeration. Shared between
/// [`scan::project_scan`] and [`position::references_of_name`] so both tools
/// agree on what counts as "the project".
pub(crate) fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".lake" | ".git" | "target" | "build" | "node_modules" | ".direnv")
}
