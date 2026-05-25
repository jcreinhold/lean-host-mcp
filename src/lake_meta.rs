//! Minimal description of a Lake project.
//!
//! [`LeanProject::open`](crate::project::LeanProject::open) consumes a
//! [`LakeProjectMeta`] to spawn a worker. Fields: canonical root,
//! toolchain label, package/library names, manifest hash, default
//! imports.
//!
//! Today the only constructor is `from_cli`, fed straight from the
//! server's clap args. A future `discover_from(hint)` will walk a
//! starting directory upward looking for `lakefile.{toml,lean}` and infer
//! these fields automatically.

use std::path::{Path, PathBuf};

use crate::error::{Result, ServerError};
use crate::index::fingerprint_lake_project;

/// Everything `LeanProject::open` needs to spawn a worker against one Lake
/// project. Constructed today from CLI args; a discovery-driven
/// constructor will replace this when multi-project resolution lands.
#[derive(Debug, Clone)]
pub struct LakeProjectMeta {
    pub canonical_root: PathBuf,
    pub toolchain: String,
    pub package: String,
    pub library: String,
    pub manifest_hash: String,
    pub default_imports: Vec<String>,
}

impl LakeProjectMeta {
    /// Build from CLI args. Canonicalises `lake_root`, reads the
    /// `lean-toolchain` file, and fingerprints the Lake manifest.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::BadProject` if `lake_root` does not
    /// canonicalise; propagates `ServerError::Index` if
    /// [`fingerprint_lake_project`] fails to read `lake-manifest.json`.
    pub fn from_cli(lake_root: &Path, package: String, library: String, imports: Vec<String>) -> Result<Self> {
        let canonical_root = lake_root
            .canonicalize()
            .map_err(|e| ServerError::BadProject(format!("canonicalise {}: {e}", lake_root.display())))?;
        let toolchain = read_lean_toolchain(&canonical_root);
        let manifest_hash = fingerprint_lake_project(&canonical_root)?;
        Ok(Self {
            canonical_root,
            toolchain,
            package,
            library,
            manifest_hash,
            default_imports: imports,
        })
    }
}

/// Contents of `<root>/lean-toolchain`, trimmed. `"unknown"` if absent —
/// matches the prior behaviour from `session.rs::lean_toolchain_label`.
fn read_lean_toolchain(root: &Path) -> String {
    let path = root.join("lean-toolchain");
    std::fs::read_to_string(&path)
        .ok()
        .map_or_else(|| "unknown".into(), |s| s.trim().to_owned())
}
