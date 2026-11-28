//! What the server knows about a Lake project before Lake runs: its root, its
//! toolchain pin and a fingerprint of its manifest.
//!
//! The server reads no lakefile and parses no manifest. Lake evaluates the
//! configuration itself, in `lake setup-file`, `lake env` and the module
//! options reader, so nothing here can disagree with it. A root must hold a
//! `lakefile.toml` or `lakefile.lean`.

use std::path::{Path, PathBuf};

use crate::error::{Result, ServerError};

/// What the private project runtime needs to spawn a worker against one Lake
/// project.
///
/// Built from a directory by [`LakeProjectMeta::from_explicit`]. Equality is
/// derived because it states the memo's contract: a value the broker serves
/// from its cache must equal what a fresh build would produce.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LakeProjectMeta {
    pub canonical_root: PathBuf,
    pub toolchain: String,
    pub manifest_hash: String,
}

/// The configuration files Lake looks for in a package root.
const LAKEFILES: [&str; 2] = ["lakefile.toml", "lakefile.lean"];

impl LakeProjectMeta {
    /// Build from an explicit Lake-root path. Canonicalises, checks that a
    /// lakefile exists, reads the toolchain pin, and fingerprints the Lake
    /// manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the path does not
    /// canonicalise, holds no lakefile, or its Lake manifest cannot be read.
    pub fn from_explicit(root: &Path) -> Result<Self> {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| ServerError::BadProject(format!("canonicalise {}: {e}", root.display())))?;
        if !LAKEFILES.iter().any(|name| canonical_root.join(name).is_file()) {
            return Err(ServerError::BadProject(format!(
                "no lakefile.toml or lakefile.lean under {}",
                canonical_root.display()
            )));
        }
        let toolchain = read_lean_toolchain(&canonical_root);
        let manifest_hash = fingerprint_lake_project(&canonical_root)?;
        Ok(Self {
            canonical_root,
            toolchain,
            manifest_hash,
        })
    }

    /// Stat every on-disk input this value was derived from, so a caller can
    /// tell whether rebuilding it would produce anything different.
    ///
    /// A warm module query is answered from a cache in tens of microseconds,
    /// so re-hashing the manifest on every call would dominate the very path
    /// the cache makes fast; four `stat` calls cost a few microseconds.
    ///
    /// The file set is exactly [`Self::from_explicit`]'s inputs — both
    /// lakefile spellings, the toolchain pin and the manifest — so an equal
    /// stamp means an equal rebuild.
    pub(crate) fn input_stamp(&self) -> ProjectInputStamp {
        input_stamp(&self.canonical_root)
    }
}

/// Stat the files at `root` that Lake configures its workspace from, and
/// that [`LakeProjectMeta::from_explicit`] reads: both lakefile spellings,
/// the toolchain pin and the manifest.
///
/// Change detection is `(length, mtime)` per file, not content. `mtime` is
/// nanosecond-resolution on APFS and ext4, so a write between two stamps is
/// always visible; length alone would not be enough, because a
/// `lake-manifest.json` revision swap is length-preserving.
pub(crate) fn input_stamp(root: &Path) -> ProjectInputStamp {
    ProjectInputStamp {
        files: [
            stat_stamp(&root.join(LAKEFILES[0])),
            stat_stamp(&root.join(LAKEFILES[1])),
            stat_stamp(&root.join("lean-toolchain")),
            stat_stamp(&root.join("lake-manifest.json")),
        ],
    }
}

/// Opaque fingerprint of the files one [`LakeProjectMeta`] was built from.
/// Produced by [`LakeProjectMeta::input_stamp`]; only equality is meaningful.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectInputStamp {
    /// `None` for a file that does not exist — its appearance or disappearance
    /// is itself a change.
    files: [Option<(u64, std::time::SystemTime)>; 4],
}

fn stat_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// Contents of `<root>/lean-toolchain`, trimmed. `"unknown"` if absent.
fn read_lean_toolchain(root: &Path) -> String {
    let path = root.join("lean-toolchain");
    std::fs::read_to_string(&path)
        .ok()
        .map_or_else(|| "unknown".into(), |s| s.trim().to_owned())
}

/// SHA-256 (lowercase hex) of `<lake_root>/lake-manifest.json`.
///
/// The manifest pins every transitive dependency revision, so its hash is a
/// tight upper bound on "is the declaration set still the same" — the staleness
/// fingerprint the broker uses to decide when a project's cached module queries
/// are stale.
///
/// Private because the broker reads it through [`LakeProjectMeta::manifest_hash`],
/// which it obtains from a stamp-validated cache; a caller hashing the manifest
/// directly would reintroduce the per-call read this module exists to avoid.
///
/// # Errors
///
/// Returns [`ServerError::BadProject`] if the manifest cannot be read.
fn fingerprint_lake_project(lake_root: &Path) -> Result<String> {
    let manifest = lake_root.join("lake-manifest.json");
    let bytes =
        std::fs::read(&manifest).map_err(|e| ServerError::BadProject(format!("read {}: {e}", manifest.display())))?;
    Ok(crate::digest::sha256_hex(&bytes))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code uses unwrap/expect/panic to surface failure paths concisely"
)]
mod tests {
    use std::fs;

    use super::*;

    /// Write a project whose every stamped input exists, so a test that
    /// mutates one of them is measuring that file and not its creation.
    fn write_project(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("lakefile.lean"), "package proj\nlean_lib Proj\n").unwrap();
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
        fs::write(root.join("lake-manifest.json"), "{\"packages\": []}\n").unwrap();
    }

    fn stamp_of(root: &Path) -> ProjectInputStamp {
        LakeProjectMeta::from_explicit(root).unwrap().input_stamp()
    }

    /// The whole soundness argument for the broker's metadata memo is that an
    /// equal stamp implies an equal rebuild, so the stamp has to be steady
    /// across a no-op: a stamp that varied on its own would make the memo a
    /// pure cost with no hits.
    #[test]
    fn input_stamp_is_unchanged_when_no_input_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write_project(&root);
        assert_eq!(stamp_of(&root), stamp_of(&root));
    }

    /// The other half of that argument, one input at a time. A stamp blind to
    /// any single file would let the broker serve metadata derived from a
    /// version of that file which no longer exists — the toolchain pin picks
    /// the worker binary and the manifest hash gates project respawn, so each
    /// omission is a distinct live bug rather than a stylistic gap.
    #[test]
    fn input_stamp_changes_when_any_input_file_changes() {
        for file in ["lakefile.lean", "lean-toolchain", "lake-manifest.json"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("proj");
            write_project(&root);
            let before = stamp_of(&root);

            let path = root.join(file);
            let mut bytes = fs::read(&path).unwrap();
            bytes.push(b'\n');
            fs::write(&path, &bytes).unwrap();

            assert_ne!(before, stamp_of(&root), "stamp ignores changes to {file}");
        }
    }

    /// A file appearing is a change even though nothing was edited: Lake
    /// prefers `lakefile.toml` over `lakefile.lean`.
    #[test]
    fn input_stamp_changes_when_a_lakefile_toml_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write_project(&root);
        let before = stamp_of(&root);

        fs::write(
            root.join("lakefile.toml"),
            "name = \"other\"\n[[lean_lib]]\nname = \"Other\"\n",
        )
        .unwrap();

        assert_ne!(before, stamp_of(&root));
    }

    /// The case where length carries no signal and `mtime` is the sole guard.
    /// Swapping one pinned revision for another in `lake-manifest.json` is
    /// length-preserving, and it is exactly the edit the manifest hash exists
    /// to catch. This test therefore also asserts the filesystem's `mtime`
    /// resolution is fine enough to separate two writes — where it is not, the
    /// memo is unsound and this failing is the correct outcome.
    #[test]
    fn input_stamp_changes_on_a_length_preserving_manifest_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        write_project(&root);
        let manifest = root.join("lake-manifest.json");
        fs::write(&manifest, "{\"rev\": \"aaaaaaa\"}\n").unwrap();
        let before = stamp_of(&root);

        fs::write(&manifest, "{\"rev\": \"bbbbbbb\"}\n").unwrap();

        assert_ne!(before, stamp_of(&root));
    }
}
