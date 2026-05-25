//! Description of a Lake project plus the discovery logic that builds one
//! from a directory hint.
//!
//! [`LeanProject::open`](crate::project::LeanProject::open) consumes a
//! [`LakeProjectMeta`] to spawn a worker. The struct carries every
//! per-project field the worker / index / cache layers need: canonical
//! root, toolchain label, package/library names, the umbrella module Lake
//! generates next to the library, manifest hash, and the default-import
//! list every fresh session is opened against.
//!
//! Two constructors:
//!
//! - [`LakeProjectMeta::from_explicit`] — caller already has a Lake-root
//!   path (e.g. resolved through the broker's [`ProjectHint::Explicit`]).
//! - [`LakeProjectMeta::discover_from`] — start from a hint and walk
//!   upward looking for `lakefile.{toml,lean}`. Used by the broker's
//!   cwd-walk step.
//!
//! Lakefile parsing is intentionally minimal — `lakefile.toml` is parsed
//! against a small `serde` shape; `lakefile.lean` falls back to a regex
//! sniff for `package <name>` and `lean_lib <Name>`. The two existing
//! fixtures (`fixtures/lean/lakefile.lean` and any TOML-based
//! project) are the calibration target. Anything more elaborate is the
//! user's job to declare via the explicit hint.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, ServerError};
use crate::index::fingerprint_lake_project;

/// Everything `LeanProject::open` needs to spawn a worker against one Lake
/// project.
///
/// Built from a directory by [`LakeProjectMeta::from_explicit`] or —
/// when the caller only has a starting hint —
/// [`LakeProjectMeta::discover_from`].
#[derive(Debug, Clone)]
pub struct LakeProjectMeta {
    pub canonical_root: PathBuf,
    pub toolchain: String,
    pub package: String,
    pub library: String,
    /// Module name Lake exposes alongside the library (the file
    /// `<root>/<Library>.lean` when present). Drives the default-import
    /// list. `None` when no umbrella file exists on disk.
    pub umbrella_module: Option<String>,
    pub manifest_hash: String,
    pub default_imports: Vec<String>,
}

impl LakeProjectMeta {
    /// Build from an explicit Lake-root path. Canonicalises, discovers the
    /// project's lakefile (TOML preferred, Lean fallback), reads the
    /// toolchain pin, and fingerprints the Lake manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the path does not
    /// canonicalise, neither lakefile variant is present, or both parsers
    /// reject the file. Propagates [`ServerError::Index`] from
    /// [`fingerprint_lake_project`] if the manifest cannot be read.
    pub fn from_explicit(root: &Path) -> Result<Self> {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| ServerError::BadProject(format!("canonicalise {}: {e}", root.display())))?;
        Self::build_from_canonical(canonical_root)
    }

    /// Walk upward from `hint` (defaulting to the current directory when
    /// `None`) looking for `lakefile.toml` or `lakefile.lean`, then behave
    /// like [`Self::from_explicit`] on the directory that contained it.
    ///
    /// # Errors
    ///
    /// As [`Self::from_explicit`], plus [`ServerError::BadProject`] when
    /// no lakefile is found between `hint` and the filesystem root.
    pub fn discover_from(hint: Option<&Path>) -> Result<Self> {
        let start = match hint {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir()
                .map_err(|e| ServerError::BadProject(format!("cannot read current directory: {e}")))?,
        };
        let found = walk_up(&start).ok_or_else(|| {
            ServerError::BadProject(format!("no lakefile.toml or lakefile.lean found from {}", start.display()))
        })?;
        Self::from_explicit(&found)
    }

    fn build_from_canonical(canonical_root: PathBuf) -> Result<Self> {
        let parsed = parse_lakefile(&canonical_root)?;
        let toolchain = read_lean_toolchain(&canonical_root);
        let manifest_hash = fingerprint_lake_project(&canonical_root)?;

        let library = parsed
            .library
            .unwrap_or_else(|| pascal_case(&parsed.package));
        let umbrella_module = umbrella_for(&canonical_root, &library);
        let default_imports = umbrella_module.clone().map_or_else(Vec::new, |m| vec![m]);

        Ok(Self {
            canonical_root,
            toolchain,
            package: parsed.package,
            library,
            umbrella_module,
            manifest_hash,
            default_imports,
        })
    }
}

/// Ascend from `start` until `lakefile.toml` or `lakefile.lean` is found.
/// Returns the directory that contains the lakefile, or `None` at root.
fn walk_up(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join("lakefile.toml").is_file() || dir.join("lakefile.lean").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// Parsed lakefile fields. `library` is optional — when absent we fall
/// back to `pascal_case(package)`.
struct LakefileParsed {
    package: String,
    library: Option<String>,
}

fn parse_lakefile(root: &Path) -> Result<LakefileParsed> {
    let toml_path = root.join("lakefile.toml");
    if toml_path.is_file() {
        return parse_lakefile_toml(&toml_path);
    }
    let lean_path = root.join("lakefile.lean");
    if lean_path.is_file() {
        return parse_lakefile_lean(&lean_path);
    }
    Err(ServerError::BadProject(format!(
        "no lakefile.toml or lakefile.lean under {}",
        root.display()
    )))
}

#[derive(Deserialize)]
struct LakefileTomlShape {
    name: String,
    #[serde(default, rename = "lean_lib")]
    lean_lib: Vec<LeanLibShape>,
}

#[derive(Deserialize)]
struct LeanLibShape {
    name: String,
}

fn parse_lakefile_toml(path: &Path) -> Result<LakefileParsed> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| ServerError::BadProject(format!("read {}: {e}", path.display())))?;
    let shape: LakefileTomlShape = toml::from_str(&contents)
        .map_err(|e| ServerError::BadProject(format!("parse {}: {e}", path.display())))?;
    Ok(LakefileParsed {
        package: shape.name,
        library: shape.lean_lib.into_iter().next().map(|l| l.name),
    })
}

fn parse_lakefile_lean(path: &Path) -> Result<LakefileParsed> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| ServerError::BadProject(format!("read {}: {e}", path.display())))?;
    // Lake accepts both bare and french-quoted identifiers
    // (`package foo` and `package «foo»`); guillemets are required when the
    // name isn't a plain Lean identifier. Match either form.
    let package_re = regex::Regex::new(r"(?m)^\s*package\s+«?([A-Za-z_][A-Za-z0-9_]*)»?")
        .map_err(|e| ServerError::Internal(format!("compile package regex: {e}")))?;
    let lean_lib_re = regex::Regex::new(r"(?m)^\s*lean_lib\s+«?([A-Za-z_][A-Za-z0-9_]*)»?")
        .map_err(|e| ServerError::Internal(format!("compile lean_lib regex: {e}")))?;
    let package = package_re
        .captures(&contents)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
        .or_else(|| {
            // Fallback: derive from the directory name. Lake projects
            // without an explicit `package` keyword default to the
            // directory name; mirror that.
            path.parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .map(default_package_from_dir_name)
        })
        .ok_or_else(|| ServerError::BadProject(format!("could not find package name in {}", path.display())))?;
    let library = lean_lib_re
        .captures(&contents)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned());
    Ok(LakefileParsed { package, library })
}

/// Lake convention: a library `Foo` looks for `Foo.lean` at the root as
/// its umbrella. Return that module name when the file exists.
fn umbrella_for(root: &Path, library: &str) -> Option<String> {
    let candidate = root.join(format!("{library}.lean"));
    if candidate.is_file() {
        Some(library.to_owned())
    } else {
        None
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

/// Sanitise a directory name into a Lake package identifier.
fn default_package_from_dir_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Snake-case → `PascalCase`. Used as the library-name fallback when a
/// lakefile declares no `lean_lib`.
fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
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

    #[test]
    fn walk_up_finds_lakefile_in_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("lakefile.lean"), "package proj\nlean_lib Proj\n").unwrap();

        let found = walk_up(&nested).expect("walk_up should find the lakefile above");
        assert_eq!(found, project);
    }

    #[test]
    fn walk_up_returns_none_when_no_lakefile_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert!(walk_up(&nested).is_none());
    }

    #[test]
    fn parse_lakefile_toml_reads_name_and_first_lean_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lakefile.toml");
        fs::write(
            &path,
            "name = \"my_project\"\n\
             [[lean_lib]]\n\
             name = \"MyProject\"\n\
             [[lean_lib]]\n\
             name = \"Other\"\n",
        )
        .unwrap();
        let parsed = parse_lakefile_toml(&path).unwrap();
        assert_eq!(parsed.package, "my_project");
        assert_eq!(parsed.library.as_deref(), Some("MyProject"));
    }

    #[test]
    fn parse_lakefile_lean_extracts_package_and_lib() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lakefile.lean");
        fs::write(
            &path,
            "import Lake\nopen Lake DSL\n\n\
             package lean_rs_fixture\n\
             lean_lib LeanRsFixture\n",
        )
        .unwrap();
        let parsed = parse_lakefile_lean(&path).unwrap();
        assert_eq!(parsed.package, "lean_rs_fixture");
        assert_eq!(parsed.library.as_deref(), Some("LeanRsFixture"));
    }

    #[test]
    fn parse_lakefile_lean_handles_french_quoted_identifiers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("lakefile.lean");
        fs::write(
            &path,
            "import Lake\nopen Lake DSL\n\n\
             package «lean_rs_fixture»\n\
             @[default_target]\n\
             lean_lib «LeanRsFixture» where\n  defaultFacets := #[LeanLib.sharedFacet]\n",
        )
        .unwrap();
        let parsed = parse_lakefile_lean(&path).unwrap();
        assert_eq!(parsed.package, "lean_rs_fixture");
        assert_eq!(parsed.library.as_deref(), Some("LeanRsFixture"));
    }

    #[test]
    fn parse_lakefile_lean_falls_back_to_dir_name_when_package_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("my-thing");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lakefile.lean");
        fs::write(&path, "-- empty lakefile, no `package` keyword\n").unwrap();
        let parsed = parse_lakefile_lean(&path).unwrap();
        assert_eq!(parsed.package, "my_thing");
        assert!(parsed.library.is_none());
    }

    #[test]
    fn pascal_case_handles_snake_and_kebab_paths() {
        assert_eq!(pascal_case("lean_rs_fixture"), "LeanRsFixture");
        assert_eq!(pascal_case("foo"), "Foo");
        assert_eq!(pascal_case(""), "");
    }

    #[test]
    fn umbrella_for_returns_some_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Lib.lean"), "").unwrap();
        assert_eq!(umbrella_for(tmp.path(), "Lib").as_deref(), Some("Lib"));
        assert!(umbrella_for(tmp.path(), "Missing").is_none());
    }
}
