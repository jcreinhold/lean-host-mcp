//! Tool implementations.
//!
//! Split by what plumbing they share rather than one file per tool:
//!
//! - [`lean`] — `elaborate`, `kernel_check`, `infer_type`, `whnf`,
//!   `is_def_eq`, `hover_by_name`. All six drive a `SessionHost` and
//!   project Lean responses into the JSON envelope.
//! - [`scan`] — `project_scan`. No Lean dependency; pure filesystem walk
//!   with a configurable regex.
//!
//! Tools that need to *enumerate* declarations (`find_symbol`,
//! `find_lemma`, `outline`) are deferred until `lean-rs` exposes a
//! `LeanName → String` rendering shim. The published 0.1.x has no such
//! path; the index would have nothing to populate.

pub mod lean;
pub mod scan;

use crate::envelope::Freshness;
use crate::session::SessionHost;

/// Shared state every tool handler reads.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub host: SessionHost,
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
