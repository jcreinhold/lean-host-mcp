//! Tool implementations.
//!
//! Split by what plumbing they share rather than one file per tool:
//!
//! - [`lean`]: term/meta tools. These drive the project's worker actor and
//!   project Lean responses into the JSON envelope.
//! - [`declaration`]: `inspect_declaration`, the bounded single-declaration
//!   proof-work inspection tool.
//! - [`proof_search`]: `search_for_proof`, the proof-agent retrieval tool
//!   built from bounded proof-state and declaration-search calls.
//! - [`proof_action`]: `try_proof_step` and `verify_declaration`, the
//!   non-mutating proof action tools.
//! - [`scan`]: `project_scan`. No Lean dependency; pure filesystem walk
//!   with a configurable regex.
//! - [`position`]: `proof_state`, `lean_query`, `references_in_file`, and
//!   `references_in_project`. Bounded module queries from
//!   `lean-rs-worker`; the cache is keyed on path + content hash + query
//!   shape.

use std::sync::Arc;

pub mod declaration;
pub mod lean;
pub mod position;
pub mod proof_action;
pub mod proof_search;
pub mod scan;

use crate::broker::ProjectBroker;
use crate::envelope::Freshness;
use crate::lake_meta::LakeProjectMeta;
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
/// the project actor's stable identity: two calls to the same project see
/// the same value; eviction or manifest invalidation changes it.
pub(crate) fn freshness_for(project: &LeanProject, imports: &[String]) -> Freshness {
    project.freshness(imports)
}

/// Imports passed to worker sessions: `Init` is the internal base
/// environment, while the public freshness envelope records only the
/// caller-supplied vector.
pub(crate) fn session_imports(imports: Vec<String>) -> Vec<String> {
    if imports.iter().any(|import| import == "Init") {
        imports
    } else {
        let mut out = Vec::with_capacity(imports.len().saturating_add(1));
        out.push("Init".to_owned());
        out.extend(imports);
        out
    }
}

/// Worker-free analogue of [`freshness_for`] for tools that resolve a project
/// through [`ProjectBroker::resolve_meta`](crate::broker::ProjectBroker::resolve_meta).
/// `session_id` is a fresh UUID per call: with no actor to identify, the
/// field carries call-identity rather than actor-identity, and clients
/// comparing `session_id` across calls should not treat a change between a
/// worker-free tool and a worker-backed tool as evidence of a re-spawn.
pub(crate) fn freshness_for_meta(meta: &LakeProjectMeta) -> Freshness {
    Freshness {
        project_root: meta.canonical_root.to_string_lossy().into_owned(),
        project_hash: meta.manifest_hash.clone(),
        imports: Vec::new(),
        session_id: uuid::Uuid::new_v4().to_string(),
        lean_toolchain: meta.toolchain.clone(),
    }
}

/// Directory names skipped during `.lean` file enumeration. Shared between
/// [`scan::project_scan`] and [`position::references_in_project`] so both
/// tools agree on what counts as "the project".
pub(crate) fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".lake" | ".git" | "target" | "build" | "node_modules" | ".direnv")
}
