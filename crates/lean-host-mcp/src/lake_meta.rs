//! Description of a Lake project plus the discovery logic that builds one
//! from a directory hint.
//!
//! The private project runtime consumes [`LakeProjectMeta`] to spawn a
//! worker. The struct carries every
//! per-project field the worker / index / cache layers need: canonical
//! root, toolchain label, package/library hints, the umbrella module Lake
//! generates next to the library, and manifest hash.
//!
//! Two constructors:
//!
//! - [`LakeProjectMeta::from_explicit`]: caller already has a Lake-root
//!   path (e.g. resolved through the broker's [`ProjectHint::Explicit`](crate::broker::ProjectHint::Explicit)).
//! - [`LakeProjectMeta::discover_from`]: start from a hint and walk
//!   upward looking for `lakefile.{toml,lean}`. Used by the broker's
//!   cwd-walk step.
//!
//! Lakefile parsing is minimal: `lakefile.toml` is parsed against a small
//! `serde` shape; `lakefile.lean` falls back to a regex sniff for
//! `package <name>` and `lean_lib <Name>`. The two existing fixtures
//! (`fixtures/lean/lakefile.lean` and any TOML-based project) are the
//! calibration target. Anything more elaborate is the user's job to
//! declare via the explicit hint.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Result, ServerError};

/// Everything the private project runtime needs to spawn a worker against one
/// Lake project.
///
/// Built from a directory by [`LakeProjectMeta::from_explicit`], or—when
/// the caller only has a starting hint—[`LakeProjectMeta::discover_from`].
/// Equality is derived because it states the memo's contract: a value the
/// broker serves from its cache must equal what a fresh build would produce.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LakeProjectMeta {
    pub canonical_root: PathBuf,
    pub toolchain: String,
    /// Lake package name when it can be discovered from the lakefile.
    /// Informational only; shims-only worker bootstrap does not load a user
    /// package dylib.
    pub package: Option<String>,
    /// Primary `lean_lib` name, or the package-derived Lake default when
    /// there is no explicit `lean_lib`. Informational.
    pub library: Option<String>,
    /// Module name Lake exposes alongside the library (the file
    /// `<root>/<Library>.lean` when present). Informational; tool calls
    /// choose their own imports. `None` when no umbrella file exists on disk.
    pub umbrella_module: Option<String>,
    pub manifest_hash: String,
}

impl LakeProjectMeta {
    /// Build from an explicit Lake-root path. Canonicalises, discovers the
    /// project's lakefile (TOML preferred, Lean fallback), reads the
    /// toolchain pin, and fingerprints the Lake manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the path does not
    /// canonicalise, neither lakefile variant is present, both parsers reject
    /// the file, or [`fingerprint_lake_project`] cannot read the manifest.
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
            ServerError::BadProject(format!(
                "no lakefile.toml or lakefile.lean found from {}",
                start.display()
            ))
        })?;
        Self::from_explicit(&found)
    }

    /// Stat every on-disk input this value was derived from, so a caller can
    /// tell whether rebuilding it would produce anything different.
    ///
    /// Building a [`LakeProjectMeta`] reads five files (~55 µs); stamping them
    /// costs five `stat` calls (~6 µs). That is the whole point: a warm module
    /// query is answered from a cache in tens of microseconds, so re-parsing
    /// the lakefile and re-hashing the manifest on every call would dominate
    /// the very path they are meant to make fast.
    ///
    /// The file set is exactly [`Self::build_from_canonical`]'s inputs — both
    /// lakefile spellings, the toolchain pin, the manifest, and this value's
    /// own umbrella candidate — so an equal stamp means an equal rebuild. The
    /// umbrella path depends on the parsed library name, which is why this is
    /// a method on a built value rather than a free function on a root.
    ///
    /// Change detection is `(length, mtime)` per file, not content. `mtime` is
    /// nanosecond-resolution on APFS and ext4, so a write between two stamps is
    /// always visible; length alone would not be enough, because a
    /// `lake-manifest.json` revision swap is length-preserving.
    pub(crate) fn input_stamp(&self) -> ProjectInputStamp {
        let root = &self.canonical_root;
        ProjectInputStamp {
            files: [
                stat_stamp(&root.join("lakefile.toml")),
                stat_stamp(&root.join("lakefile.lean")),
                stat_stamp(&root.join("lean-toolchain")),
                stat_stamp(&root.join("lake-manifest.json")),
                self.library
                    .as_ref()
                    .and_then(|library| stat_stamp(&root.join(format!("{library}.lean")))),
            ],
        }
    }

    fn build_from_canonical(canonical_root: PathBuf) -> Result<Self> {
        let parsed = parse_lakefile(&canonical_root)?;
        let toolchain = read_lean_toolchain(&canonical_root);
        let manifest_hash = fingerprint_lake_project(&canonical_root)?;

        let library = parsed.library.unwrap_or_else(|| pascal_case(&parsed.package));
        let umbrella_module = umbrella_for(&canonical_root, &library);

        Ok(Self {
            canonical_root,
            toolchain,
            package: Some(parsed.package),
            library: Some(library),
            umbrella_module,
            manifest_hash,
        })
    }
}

/// Opaque fingerprint of the files one [`LakeProjectMeta`] was built from.
/// Produced by [`LakeProjectMeta::input_stamp`]; only equality is meaningful.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectInputStamp {
    /// `None` for a file that does not exist — its appearance or disappearance
    /// is itself a change (a project gaining a `lakefile.toml` beside its
    /// `lakefile.lean` parses differently).
    files: [Option<(u64, std::time::SystemTime)>; 5],
}

fn stat_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
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

/// Parsed lakefile fields. `library` is optional; when absent the fallback
/// is `pascal_case(package)`.
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
    let contents =
        std::fs::read_to_string(path).map_err(|e| ServerError::BadProject(format!("read {}: {e}", path.display())))?;
    let shape: LakefileTomlShape =
        toml::from_str(&contents).map_err(|e| ServerError::BadProject(format!("parse {}: {e}", path.display())))?;
    Ok(LakefileParsed {
        package: shape.name,
        library: shape.lean_lib.into_iter().next().map(|l| l.name),
    })
}

/// Sniff patterns for `lakefile.lean`.
///
/// Lake accepts both bare and french-quoted identifiers (`package foo` and
/// `package «foo»`); guillemets are required when the name isn't a plain Lean
/// identifier. Match either form.
///
/// Compiled once rather than per call: the patterns are string literals, so
/// either they compile on the first project opened or they never compile at
/// all — a defect no caller could act on and no runtime input can trigger.
/// That is why the failure is a panic in an initializer instead of a
/// `ServerError::Internal` a caller would have to thread through.
static PACKAGE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "literal pattern: unrepresentable at runtime, see the doc above"
    )]
    regex::Regex::new(r"(?m)^\s*package\s+«?([A-Za-z_][A-Za-z0-9_]*)»?").expect("package regex is a valid literal")
});

/// Companion of [`PACKAGE_RE`]; same rationale.
static LEAN_LIB_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "literal pattern: unrepresentable at runtime, see PACKAGE_RE"
    )]
    regex::Regex::new(r"(?m)^\s*lean_lib\s+«?([A-Za-z_][A-Za-z0-9_]*)»?").expect("lean_lib regex is a valid literal")
});

fn parse_lakefile_lean(path: &Path) -> Result<LakefileParsed> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| ServerError::BadProject(format!("read {}: {e}", path.display())))?;
    let package = PACKAGE_RE
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
    let library = LEAN_LIB_RE
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
    let digest = Sha256::digest(&bytes);
    let mut s = String::with_capacity(digest.len().saturating_mul(2));
    for byte in &digest {
        let _ = write!(s, "{byte:02x}");
    }
    Ok(s)
}

/// Project-relative directory holding built module artifacts: `.olean` for the
/// environment, `.ilean` for the reference index. One definition, because
/// [`artifact_roots`] and the `.ilean` reader must agree on where Lake writes.
pub(crate) const BUILD_LIB_REL: &str = ".lake/build/lib/lean";

/// Every directory a module this project imports could have its `.olean` built
/// into: the project's own build tree, then one per Lake package.
///
/// Computed once per project rather than per call — the package set is a
/// function of `lake-manifest.json`, and a manifest change already rebuilds the
/// whole [`LakeProjectMeta`] and evicts the project. An unbuilt or missing
/// `.lake/packages` simply contributes nothing.
pub(crate) fn artifact_roots(lake_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![lake_root.join(BUILD_LIB_REL)];
    let Ok(entries) = std::fs::read_dir(lake_root.join(".lake/packages")) else {
        return roots;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(BUILD_LIB_REL);
        if candidate.is_dir() {
            roots.push(candidate);
        }
    }
    roots
}

/// Newest modification time among the built `.olean` files of `modules`, or
/// `None` when none of them resolves to one.
///
/// This is the cheap, precise half of "did the environment these imports
/// describe change on disk": at most one `stat` per module per root, with the
/// project's own tree first so the common developer loop hits immediately. The
/// obvious alternative — the newest mtime anywhere under `.lake/build/lib` —
/// was measured at 160–190 ms warm and 1.5 s cold over mathlib4's 8408
/// `.olean` files, which is not a per-call cost worth paying.
///
/// It watches only the *named* imports, not their transitive closure. That is
/// nearly the same set in practice: Lake's traces include each dependency's
/// hash, so rebuilding a dependency rebuilds the `.olean` of everything that
/// imports it. The gap is a dependency built alone and never propagated, where
/// the importing module's own artifact is genuinely still the one last built.
///
/// `None` is returned for an unbuilt project rather than an error: nothing to
/// watch is not a failure, and a caller comparing two `None`s correctly
/// concludes nothing changed.
pub(crate) fn import_artifact_stamp(roots: &[PathBuf], modules: &[String]) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    for module in modules {
        let relative: PathBuf = module.split('.').collect::<PathBuf>().with_extension("olean");
        for root in roots {
            let Ok(metadata) = std::fs::metadata(root.join(&relative)) else {
                continue;
            };
            if let Ok(modified) = metadata.modified() {
                newest = Some(newest.map_or(modified, |current| current.max(modified)));
            }
            // First root that has this module wins: a module lives in exactly
            // one package's build tree, so later roots cannot hold a different
            // artifact for the same name.
            break;
        }
    }
    newest
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

    /// Stamp `path` with an explicit mtime. Used instead of sleeping: the
    /// contract is "newest mtime across the import set", and a real `lake
    /// build` advances it by far more than a test can afford to wait for.
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    /// Write `<root>/<BUILD_LIB_REL>/<module path>.olean`, creating parents.
    fn write_olean(root: &Path, module: &str, body: &str) -> PathBuf {
        let relative: PathBuf = module.split('.').collect::<PathBuf>().with_extension("olean");
        let path = root.join(BUILD_LIB_REL).join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn artifact_roots_put_the_project_first_and_include_built_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(BUILD_LIB_REL)).unwrap();
        fs::create_dir_all(root.join(".lake/packages/batteries").join(BUILD_LIB_REL)).unwrap();
        // A package directory that was never built contributes no root: a
        // non-directory candidate must not become a path we stat per call.
        fs::create_dir_all(root.join(".lake/packages/unbuilt")).unwrap();

        let roots = artifact_roots(root);

        assert_eq!(
            roots.first(),
            Some(&root.join(BUILD_LIB_REL)),
            "the project's own tree must be probed first; it is the developer-loop hit"
        );
        assert!(roots.contains(&root.join(".lake/packages/batteries").join(BUILD_LIB_REL)));
        assert_eq!(roots.len(), 2, "unbuilt packages must not contribute roots: {roots:?}");
    }

    #[test]
    fn artifact_roots_survive_a_project_with_no_lake_directory_at_all() {
        let tmp = tempfile::tempdir().unwrap();
        // An unbuilt project still yields its own (nonexistent) build dir, so
        // `import_artifact_stamp` has something to probe and answers `None`
        // rather than the caller having to special-case an empty root list.
        let roots = artifact_roots(tmp.path());
        assert_eq!(roots, vec![tmp.path().join(BUILD_LIB_REL)]);
        assert_eq!(
            import_artifact_stamp(&roots, &["Demo.A".to_owned()]),
            None,
            "an unbuilt project has nothing to watch"
        );
    }

    #[test]
    fn the_import_stamp_advances_when_an_imported_olean_is_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_olean(root, "Demo.A", "first");
        write_olean(root, "Demo.B", "first");
        let roots = artifact_roots(root);
        let modules = vec!["Demo.A".to_owned(), "Demo.B".to_owned()];

        let before = import_artifact_stamp(&roots, &modules).expect("both modules are built");

        // Rewrite one of the two with a strictly later mtime.
        let later = before + std::time::Duration::from_mins(1);
        let rebuilt = write_olean(root, "Demo.B", "second");
        set_mtime(&rebuilt, later);

        let after = import_artifact_stamp(&roots, &modules).expect("both modules are still built");
        assert!(
            after > before,
            "rebuilding an imported module must advance the stamp; {before:?} -> {after:?}"
        );

        // A module the call does not import is invisible, however new it is:
        // otherwise every unrelated `lake build` would cycle the worker.
        let unrelated = write_olean(root, "Demo.C", "brand new");
        set_mtime(&unrelated, later + std::time::Duration::from_mins(1));
        assert_eq!(
            import_artifact_stamp(&roots, &modules),
            Some(after),
            "a module outside the import set must not move the stamp"
        );
    }

    #[test]
    fn an_unbuilt_module_in_the_import_set_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_olean(root, "Demo.A", "built");
        let roots = artifact_roots(root);

        let built_only = import_artifact_stamp(&roots, &["Demo.A".to_owned()]);
        let with_missing = import_artifact_stamp(&roots, &["Demo.A".to_owned(), "Demo.NeverBuilt".to_owned()]);

        assert!(built_only.is_some());
        assert_eq!(
            with_missing, built_only,
            "an unbuilt import contributes nothing rather than sinking the stamp"
        );
    }

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

    /// Write a project whose every stamped input exists, so a test that
    /// mutates one of them is measuring that file and not its creation.
    fn write_project(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("lakefile.lean"), "package proj\nlean_lib Proj\n").unwrap();
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.34.0-rc2\n").unwrap();
        fs::write(root.join("lake-manifest.json"), "{\"packages\": []}\n").unwrap();
        fs::write(root.join("Proj.lean"), "-- umbrella\n").unwrap();
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
        for file in ["lakefile.lean", "lean-toolchain", "lake-manifest.json", "Proj.lean"] {
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
    /// prefers `lakefile.toml` over `lakefile.lean`, so a project gaining one
    /// parses to a different package and library.
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
