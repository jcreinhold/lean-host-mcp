//! Tool implementations.
//!
//! Split by what plumbing they share rather than one file per tool:
//!
//! - [`lean`]—`elaborate`, `kernel_check`, `infer_type`, `whnf`,
//!   `is_def_eq`, `hover_by_name`. All six drive the project's worker actor
//!   and project Lean responses into the JSON envelope.
//! - [`scan`]—`project_scan`. No Lean dependency; pure filesystem walk
//!   with a configurable regex.
//! - [`index`]—`find_symbol`, `find_lemma`, `outline`. Thin wrappers
//!   over the SQLite-backed [`DeclarationIndex`](crate::DeclarationIndex);
//!   rebuild on Lake-manifest change.
//! - [`position`]—`goal_at_position`, `type_at_position`,
//!   `references_of_name`, `file_diagnostics`. Thin lookups over a
//!   `ProcessedFileCache`-backed `LeanWorkerProcessedFile` projection from
//!   `lean-rs-worker`; the cache is keyed on path + content hash.

use std::sync::Arc;

pub mod index;
pub mod lean;
pub mod position;
pub mod scan;

use crate::broker::ProjectBroker;
use crate::envelope::Freshness;
use crate::project::LeanProject;

/// Shared state every tool handler reads.
///
/// Holds the broker; each tool's body resolves its
/// [`ProjectHint`](crate::broker::ProjectHint) inside
/// [`ProjectBroker::with_project`](crate::broker::ProjectBroker::with_project)
/// and receives an `Arc<LeanProject>` for the duration of its closure.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub broker: Arc<ProjectBroker>,
}

/// Build the [`Freshness`] envelope for a tool response. `session_id` is
/// the project actor's stable identity — two calls to the same project see
/// the same value; eviction or manifest invalidation changes it.
pub(crate) fn freshness_for(project: &LeanProject, imports: &[String]) -> Freshness {
    project.freshness(imports)
}

/// Directory names skipped during `.lean` file enumeration. Shared between
/// [`scan::project_scan`] and [`position::references_of_name`] so both tools
/// agree on what counts as "the project".
pub(crate) fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".lake" | ".git" | "target" | "build" | "node_modules" | ".direnv")
}
