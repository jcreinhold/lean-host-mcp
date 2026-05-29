//! Tool implementations.
//!
//! Split by what plumbing they share rather than one file per tool:
//!
//! - [`declaration`]: `inspect_declaration`, the bounded single-declaration
//!   proof-work inspection tool.
//! - [`proof_search`]: `search_for_proof`, the proof-agent retrieval tool
//!   built from bounded proof-state and declaration-search calls.
//! - [`proof_action`]: `try_proof_step` and `verify_declaration`, the
//!   non-mutating proof action tools.
//! - [`position`]: `proof_state` and `find_references`, backed by bounded
//!   module queries from `lean-rs-worker`.

use std::sync::Arc;

pub mod declaration;
pub mod position;
pub mod proof_action;
pub mod proof_search;
pub(crate) mod source_input;

use crate::broker::ProjectBroker;

/// Shared state every tool handler reads.
///
/// Holds the broker; each tool calls a narrow broker operation for its
/// semantic work instead of receiving a raw project actor handle.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub broker: Arc<ProjectBroker>,
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

/// Directory names skipped during `.lean` file enumeration. Shared between
/// reference scans so they agree on what counts as "the project".
pub(crate) fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".lake" | ".git" | "target" | "build" | "node_modules" | ".direnv")
}
