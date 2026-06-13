//! Pure-Rust reader for Lean's per-module `.ilean` reference indices.
//!
//! `lake build` writes one `.ilean` file per module under
//! `<project>/.lake/build/lib/lean/`. An `.ilean` is the LSP reference index:
//! for every name it records the definition site and all usage sites *within
//! that module's source*. Reading it answers "all references to a
//! fully-qualified name `N`" in milliseconds — no Lean runtime, no
//! re-elaboration — because the format is plain JSON (`Json.compress`) with
//! names stored as strings. This module therefore lives in the parent crate
//! and links nothing from Lean.
//!
//! The boundary is one query: [`references_to`] takes a project root and a
//! fully-qualified name and returns a [`ReferenceIndex`] of resolved hits
//! (source file + LSP range + def/ref kind). Everything else — file
//! enumeration, the compact-array JSON codecs, the version gate, and
//! module→source-path resolution — is hidden. The raw JSON types are private;
//! callers never see them.
//!
//! "Index absent / stale" is reported as **data**, not an error: a project
//! that was never built yields [`IndexStatus::NotBuilt`]; a single malformed
//! or unreadable `.ilean` is skipped and counted, never fatal. The consumer
//! (`find_references`) maps those signals onto the response envelope.
//!
//! Schema reference (Lean v4.31.0-rc1):
//! `src/lean/Lean/Data/Lsp/Internal.lean` (`RefIdent`, `RefInfo`, `ModuleRefs`),
//! `src/lean/Lean/Server/References.lean` (`Ilean` / `Ilean.load`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use walkdir::WalkDir;

/// Only this `.ilean` format version is understood. A different version is
/// rejected rather than parsed into a silent wrong answer.
const SUPPORTED_VERSION: u64 = 5;

/// Project-relative path to the directory holding the project's own module
/// indices. Dependency indices live under separate `.lake/packages/*/` trees,
/// so a recursive walk of this directory yields exactly the project's modules.
const BUILD_LIB_REL: &str = ".lake/build/lib/lean";

/// Whether a resolved location is a definition site or a use site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefKind {
    /// The binder/definition occurrence of the name.
    Def,
    /// A use site of the name.
    Ref,
}

/// A single reference to the queried name, resolved to a source location.
///
/// Coordinates are 0-based LSP line/column, carried straight from the index.
/// Maps directly onto `find_references`'s `ReferenceHit`.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceLocation {
    /// Resolved `<root>/Namespace/Module.lean` for the module that recorded it.
    pub file: PathBuf,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub kind: RefKind,
}

/// Whether the project's reference index exists on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexStatus {
    /// `<root>/.lake/build/lib/lean` is missing — the project was never built.
    NotBuilt,
    /// The index directory exists and was scanned.
    Present,
}

/// Outcome of a project-wide reference query. Reports as data — it never warns.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceIndex {
    pub status: IndexStatus,
    pub references: Vec<ReferenceLocation>,
    /// `.ilean` files parsed successfully.
    pub modules_scanned: usize,
    /// `.ilean` files skipped because they were unreadable, malformed, or an
    /// unsupported version. One bad file does not sink the query.
    pub modules_skipped: usize,
    /// Contributing modules whose source `.lean` is newer than its `.ilean`
    /// (the recorded locations may be stale). Bounded by the result set, not
    /// the project size.
    pub stale_sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModuleDeclarationIndexStatus {
    /// `<root>/.lake/build/lib/lean` is missing — the project was never built.
    ProjectNotBuilt,
    /// The build tree exists, but this module has no `.ilean` file.
    ModuleNotBuilt,
    /// The module index exists and was parsed.
    Present,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedDeclaration {
    pub name: String,
    pub declaration_span: DeclSpan,
    pub selection_span: DeclSpan,
}

#[derive(Debug, Clone)]
pub(crate) struct DeclSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleDeclarationIndex {
    pub status: ModuleDeclarationIndexStatus,
    pub module: String,
    pub index: PathBuf,
    pub declarations: Vec<IndexedDeclaration>,
    pub stale: bool,
}

/// A recoverable failure loading a single `.ilean` file.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IleanError {
    #[error("unsupported .ilean version {found} at {} (reader supports version {})", path.display(), SUPPORTED_VERSION)]
    UnsupportedVersion { path: PathBuf, found: u64 },
    #[error("read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Collect every reference to fully-qualified `name` in the project rooted at
/// `project_root`.
///
/// Infallible at the top level: a missing build directory yields
/// [`IndexStatus::NotBuilt`]; an individual unreadable or malformed `.ilean`
/// is counted in [`ReferenceIndex::modules_skipped`] and skipped. Never panics.
pub(crate) fn references_to(project_root: &Path, name: &str) -> ReferenceIndex {
    let build_dir = project_root.join(BUILD_LIB_REL);
    if !build_dir.is_dir() {
        return ReferenceIndex {
            status: IndexStatus::NotBuilt,
            references: Vec::new(),
            modules_scanned: 0,
            modules_skipped: 0,
            stale_sources: Vec::new(),
        };
    }

    let mut references: Vec<ReferenceLocation> = Vec::new();
    let mut modules_scanned = 0usize;
    let mut modules_skipped = 0usize;
    // (source path, index path) for each module that produced a hit; stale
    // detection only touches these, keeping the stat work off the hot path.
    let mut contributing: Vec<(PathBuf, PathBuf)> = Vec::new();

    for entry in WalkDir::new(&build_dir).into_iter().filter_map(std::result::Result::ok) {
        let index_path = entry.path();
        if !entry.file_type().is_file() || index_path.extension().is_none_or(|ext| ext != "ilean") {
            continue;
        }
        let Ok(module) = load(index_path) else {
            modules_skipped = modules_skipped.saturating_add(1);
            continue;
        };
        modules_scanned = modules_scanned.saturating_add(1);

        let source = module_to_source(project_root, &module.module);
        let before = references.len();
        for (key, info) in &module.references {
            // Cheap substring gate before the (rarer) key JSON parse, then the
            // exact const-name check.
            if !key.contains(name) || !is_const_named(key, name) {
                continue;
            }
            if let Some(definition) = &info.definition {
                references.push(location_hit(&source, definition, RefKind::Def));
            }
            for usage in &info.usages {
                references.push(location_hit(&source, usage, RefKind::Ref));
            }
        }
        if references.len() > before {
            contributing.push((source, index_path.to_path_buf()));
        }
    }

    let stale_sources = collect_stale_sources(&contributing);

    ReferenceIndex {
        status: IndexStatus::Present,
        references,
        modules_scanned,
        modules_skipped,
        stale_sources,
    }
}

pub(crate) fn declarations_in_module(project_root: &Path, module: &str) -> ModuleDeclarationIndex {
    let build_dir = project_root.join(BUILD_LIB_REL);
    let source = module_to_source(project_root, module);
    let index = module_to_index(project_root, module);
    if !build_dir.is_dir() {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ProjectNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
            stale: false,
        };
    }
    if !index.is_file() {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ModuleNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
            stale: false,
        };
    }
    let Ok(raw) = load(&index) else {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ModuleNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
            stale: false,
        };
    };
    let mut declarations = raw
        .decls
        .into_iter()
        .map(|(name, info)| IndexedDeclaration {
            name,
            declaration_span: decl_span(&info.range),
            selection_span: decl_span(&info.selection_range),
        })
        .collect::<Vec<_>>();
    declarations.sort_by(|a, b| {
        a.declaration_span
            .start_line
            .cmp(&b.declaration_span.start_line)
            .then(a.declaration_span.start_column.cmp(&b.declaration_span.start_column))
            .then(a.name.cmp(&b.name))
    });
    let stale = source_newer_than_index(&source, &index);
    ModuleDeclarationIndex {
        status: ModuleDeclarationIndexStatus::Present,
        module: raw.module,
        index,
        declarations,
        stale,
    }
}

/// Read, version-gate, and parse one `.ilean` file.
///
/// # Errors
///
/// [`IleanError::Io`] when the file cannot be read, [`IleanError::Json`] on a
/// parse failure, and [`IleanError::UnsupportedVersion`] when the file's
/// `version` is not [`SUPPORTED_VERSION`] — checked before the full parse so an
/// unknown format never yields a wrong answer.
fn load(path: &Path) -> Result<IleanRaw, IleanError> {
    let bytes = std::fs::read(path).map_err(|source| IleanError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Probe `version` first: the struct ignores every other field, so this is
    // a near-free pass that gates before we trust the rest of the document.
    let probe: VersionProbe = serde_json::from_slice(&bytes).map_err(|source| IleanError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    if probe.version != SUPPORTED_VERSION {
        return Err(IleanError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: probe.version,
        });
    }
    serde_json::from_slice(&bytes).map_err(|source| IleanError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Resolve a dotted module name to its source path: `Demo.A` →
/// `<root>/Demo/A.lean`.
fn module_to_source(root: &Path, module: &str) -> PathBuf {
    let relative: PathBuf = module.split('.').collect();
    root.join(relative).with_extension("lean")
}

fn module_to_index(root: &Path, module: &str) -> PathBuf {
    let relative: PathBuf = module.split('.').collect();
    root.join(BUILD_LIB_REL).join(relative).with_extension("ilean")
}

/// True when `key` is a `const` `RefIdent` whose identifier equals `name`.
///
/// The key is itself a compressed-JSON object — the externally-tagged
/// `RefIdent`: `{"c":{"m":definingModule,"n":ident}}` for a global name or
/// `{"f":{"m":module,"i":id}}` for a local fvar. Only `const` keys participate
/// in find-references; fvars and malformed keys are ignored.
fn is_const_named(key: &str, name: &str) -> bool {
    match serde_json::from_str::<RefIdentKey>(key) {
        Ok(RefIdentKey::Const { n }) => n == name,
        Ok(RefIdentKey::Fvar {}) | Err(_) => false,
    }
}

/// Project a raw index location onto a resolved [`ReferenceLocation`].
fn location_hit(source: &Path, location: &LocationRaw, kind: RefKind) -> ReferenceLocation {
    ReferenceLocation {
        file: source.to_path_buf(),
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
        kind,
    }
}

/// Flag contributing sources whose `.lean` is newer than its `.ilean`.
/// Best-effort: a stat failure on either side is treated as "not stale".
fn collect_stale_sources(contributing: &[(PathBuf, PathBuf)]) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    for (source, index) in contributing {
        if source_newer_than_index(source, index) {
            stale.push(source.clone());
        }
    }
    stale
}

fn source_newer_than_index(source: &Path, index: &Path) -> bool {
    let Ok(source_mtime) = std::fs::metadata(source).and_then(|meta| meta.modified()) else {
        return false;
    };
    let Ok(index_mtime) = std::fs::metadata(index).and_then(|meta| meta.modified()) else {
        return false;
    };
    source_mtime > index_mtime
}

// === Private raw JSON shapes ===============================================

/// Minimal first-pass shape used only to read and gate on `version`. Serde
/// ignores `module`/`references`/`decls`/`directImports`.
#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
}

/// The compressed-JSON `RefIdent` used as a `references` map key, parsed from
/// its externally-tagged form. Only the const identifier `n` is read; the
/// defining module and the fvar payload are intentionally ignored.
#[derive(Deserialize)]
enum RefIdentKey {
    #[serde(rename = "c")]
    Const { n: String },
    #[serde(rename = "f")]
    Fvar {},
}

/// The fields of an `.ilean` document this reader consumes. `decls` and
/// `directImports` are deliberately not modeled (serde ignores unknown
/// fields) — paying to deserialize them would be waste.
#[derive(Deserialize)]
struct IleanRaw {
    /// Dotted module name, e.g. `KanProofs.Foo.Bar`.
    module: String,
    /// Compressed-`RefIdent` key → reference info.
    references: std::collections::HashMap<String, RefInfoRaw>,
    #[serde(default)]
    decls: BTreeMap<String, DeclInfoRaw>,
}

/// Definition site (optional) and usage sites of one reference.
#[derive(Deserialize)]
struct RefInfoRaw {
    /// `null` when this module is not the definition site.
    #[serde(default)]
    definition: Option<LocationRaw>,
    #[serde(default)]
    usages: Vec<LocationRaw>,
}

/// A reference location, stored in the index as a 4- or 5-element array:
/// `[startLine, startCol, endLine, endCol]` with an optional 5th `parentDecl`
/// string that this reader discards.
struct LocationRaw {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

struct DeclInfoRaw {
    range: DeclInfoRangeRaw,
    selection_range: DeclInfoRangeRaw,
}

struct DeclInfoRangeRaw {
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

fn decl_span(range: &DeclInfoRangeRaw) -> DeclSpan {
    DeclSpan {
        start_line: range.start_line,
        start_column: range.start_column,
        end_line: range.end_line,
        end_column: range.end_column,
    }
}

impl<'de> Deserialize<'de> for DeclInfoRaw {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DeclInfoVisitor;

        impl<'de> Visitor<'de> for DeclInfoVisitor {
            type Value = DeclInfoRaw;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an 8-element .ilean declaration info array")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<DeclInfoRaw, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let range = DeclInfoRangeRaw {
                    start_line: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(0, &self))?,
                    start_column: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(1, &self))?,
                    end_line: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(2, &self))?,
                    end_column: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(3, &self))?,
                };
                let selection_range = DeclInfoRangeRaw {
                    start_line: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(4, &self))?,
                    start_column: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(5, &self))?,
                    end_line: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(6, &self))?,
                    end_column: seq.next_element()?.ok_or_else(|| de::Error::invalid_length(7, &self))?,
                };
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(9, &self));
                }
                Ok(DeclInfoRaw { range, selection_range })
            }
        }

        deserializer.deserialize_seq(DeclInfoVisitor)
    }
}

impl<'de> Deserialize<'de> for LocationRaw {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LocationVisitor;

        impl<'de> Visitor<'de> for LocationVisitor {
            type Value = LocationRaw;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a 4- or 5-element .ilean location array")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<LocationRaw, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let start_line = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let start_column = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let end_line = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let end_column = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(3, &self))?;
                // A 5th element (parentDecl) is permitted and ignored; a 6th
                // means a shape this reader does not recognize.
                let _parent_decl: Option<de::IgnoredAny> = seq.next_element()?;
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(6, &self));
                }
                Ok(LocationRaw {
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                })
            }
        }

        deserializer.deserialize_seq(LocationVisitor)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test code uses unwrap/expect/panic to surface failure paths concisely"
)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    /// Read a committed fixture body. The `.lake/` directory is gitignored, so
    /// the raw `.ilean` JSON bodies are committed as flat files and staged into
    /// the real on-disk layout per test (see [`stage`]).
    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ilean")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()))
    }

    /// Materialize a project root in a tempdir: each `(module, fixture)` pair
    /// writes the fixture body to `<root>/.lake/build/lib/lean/<Mod/Path>.ilean`
    /// and a matching source stub at `<root>/<Mod/Path>.lean`.
    fn stage(modules: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join(BUILD_LIB_REL);
        for (module, fixture_name) in modules {
            let relative: PathBuf = module.split('.').collect();

            let index = build.join(&relative).with_extension("ilean");
            fs::create_dir_all(index.parent().unwrap()).unwrap();
            fs::write(&index, fixture(fixture_name)).unwrap();

            let source = tmp.path().join(&relative).with_extension("lean");
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, "-- source stub\n").unwrap();
        }
        tmp
    }

    fn hit(index: &ReferenceIndex, file_suffix: &str, kind: RefKind) -> Vec<(u32, u32, u32, u32)> {
        index
            .references
            .iter()
            .filter(|reference| reference.kind == kind && reference.file.ends_with(file_suffix))
            .map(|reference| {
                (
                    reference.start_line,
                    reference.start_column,
                    reference.end_line,
                    reference.end_column,
                )
            })
            .collect()
    }

    #[test]
    fn resolves_def_and_usages_across_modules() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");

        assert_eq!(index.status, IndexStatus::Present);
        assert_eq!(index.modules_scanned, 2);
        assert_eq!(index.modules_skipped, 0);

        // Definition lives in Demo.A's source; module→source resolution is
        // asserted by the file suffix.
        assert_eq!(hit(&index, "Demo/A.lean", RefKind::Def), vec![(3, 4, 3, 7)]);
        assert!(hit(&index, "Demo/A.lean", RefKind::Ref).is_empty());

        // Both usages live in Demo.B's source (one carries a 5th parentDecl
        // element, which must be parsed and discarded).
        assert!(hit(&index, "Demo/B.lean", RefKind::Def).is_empty());
        let mut b_refs = hit(&index, "Demo/B.lean", RefKind::Ref);
        b_refs.sort_unstable();
        assert_eq!(b_refs, vec![(5, 2, 5, 5), (6, 8, 6, 11)]);

        assert_eq!(index.references.len(), 3);
    }

    #[test]
    fn ignores_fvar_keys_and_mismatched_consts() {
        // demo_b.ilean carries an fvar key whose id is "Demo.A.foo" and a const
        // usage of "Init.Nat" — neither must surface for the queried name.
        let project = stage(&[("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");

        // Only the two const usages of Demo.A.foo, none from the fvar entry.
        assert_eq!(index.references.len(), 2);
        assert!(index.references.iter().all(|reference| reference.kind == RefKind::Ref));
    }

    #[test]
    fn unknown_version_is_rejected_with_typed_error() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ilean/bad_version.ilean");
        match load(&path) {
            Err(IleanError::UnsupportedVersion { found, .. }) => assert_eq!(found, 99),
            Err(other) => panic!("expected UnsupportedVersion, got error {other:?}"),
            Ok(_) => panic!("expected UnsupportedVersion, got a successful load"),
        }
    }

    #[test]
    fn malformed_file_is_skipped_not_fatal() {
        // load() surfaces a typed Json error directly...
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ilean/malformed.ilean");
        assert!(matches!(load(&path), Err(IleanError::Json { .. })));

        // ...and a project query counts it as skipped while still resolving the
        // good module alongside it.
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.Bad", "malformed.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");
        assert_eq!(index.status, IndexStatus::Present);
        assert_eq!(index.modules_scanned, 1);
        assert_eq!(index.modules_skipped, 1);
        assert_eq!(hit(&index, "Demo/A.lean", RefKind::Def), vec![(3, 4, 3, 7)]);
    }

    #[test]
    fn null_definition_yields_only_usages() {
        // Demo.B's entry for Demo.A.foo has a null definition.
        let project = stage(&[("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");
        assert!(index.references.iter().all(|reference| reference.kind == RefKind::Ref));
    }

    #[test]
    fn location_array_arity_is_bounded() {
        // 4 elements: bare range.
        assert!(serde_json::from_str::<LocationRaw>("[1,2,3,4]").is_ok());
        // 5 elements: trailing parentDecl, discarded.
        assert!(serde_json::from_str::<LocationRaw>("[1,2,3,4,\"D.bar\"]").is_ok());
        // Too few / too many are rejected.
        assert!(serde_json::from_str::<LocationRaw>("[1,2,3]").is_err());
        assert!(serde_json::from_str::<LocationRaw>("[1,2,3,4,\"D.bar\",9]").is_err());
        assert!(serde_json::from_str::<LocationRaw>("[1,2,3,4,5,6,7]").is_err());
    }

    #[test]
    fn unbuilt_project_reports_not_built() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("Demo")).unwrap();
        fs::write(tmp.path().join("Demo/A.lean"), "-- no build\n").unwrap();

        let index = references_to(tmp.path(), "Demo.A.foo");
        assert_eq!(index.status, IndexStatus::NotBuilt);
        assert!(index.references.is_empty());
        assert_eq!(index.modules_scanned, 0);
    }

    #[test]
    fn stale_source_is_flagged() {
        // Stage normally, then bump the contributing source's mtime past its
        // index. A short sleep guarantees a distinct mtime regardless of
        // filesystem timestamp resolution.
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        std::thread::sleep(Duration::from_millis(20));
        let source = project.path().join("Demo/A.lean");
        fs::write(&source, "-- edited after build\n").unwrap();

        let index = references_to(project.path(), "Demo.A.foo");
        let stale: BTreeSet<_> = index.stale_sources.iter().collect();
        assert!(
            stale.contains(&source),
            "expected {source:?} flagged stale, got {stale:?}"
        );
    }

    #[test]
    fn declarations_in_module_reads_decl_ranges_from_ilean() {
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        let index = declarations_in_module(project.path(), "Demo.A");

        assert_eq!(index.status, ModuleDeclarationIndexStatus::Present);
        assert_eq!(index.declarations.len(), 1);
        let declaration = index.declarations.first().unwrap();
        assert_eq!(declaration.name, "Demo.A.foo");
        assert_eq!(declaration.declaration_span.start_line, 3);
        assert_eq!(declaration.selection_span.start_column, 4);
    }

    #[test]
    fn declarations_in_module_reports_missing_module_index() {
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        let index = declarations_in_module(project.path(), "Demo.Missing");

        assert_eq!(index.status, ModuleDeclarationIndexStatus::ModuleNotBuilt);
        assert!(index.declarations.is_empty());
    }

    #[test]
    fn declarations_in_module_reports_project_not_built() {
        let tmp = tempfile::tempdir().unwrap();
        let index = declarations_in_module(tmp.path(), "Demo.A");

        assert_eq!(index.status, ModuleDeclarationIndexStatus::ProjectNotBuilt);
        assert!(index.declarations.is_empty());
    }

    #[test]
    fn declaration_info_array_arity_is_exact() {
        assert!(serde_json::from_str::<DeclInfoRaw>("[1,2,3,4,5,6,7,8]").is_ok());
        assert!(serde_json::from_str::<DeclInfoRaw>("[1,2,3,4,5,6,7]").is_err());
        assert!(serde_json::from_str::<DeclInfoRaw>("[1,2,3,4,5,6,7,8,9]").is_err());
    }

    /// Measurement, not a gate. Point at a real built project to sanity-check
    /// shape and timing:
    ///
    /// ```sh
    /// LEAN_HOST_MCP_ILEAN_FIXTURE=~/Code/kan-proofs \
    /// LEAN_HOST_MCP_ILEAN_NAME=<FQN> \
    /// cargo test -p lean-host-mcp ilean -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires a built project via LEAN_HOST_MCP_ILEAN_FIXTURE"]
    fn timing_against_real_index() {
        let Some(root) = std::env::var_os("LEAN_HOST_MCP_ILEAN_FIXTURE") else {
            eprintln!("LEAN_HOST_MCP_ILEAN_FIXTURE unset; skipping");
            return;
        };
        let name = std::env::var("LEAN_HOST_MCP_ILEAN_NAME").unwrap_or_else(|_| "Nat.add".to_owned());
        let started = std::time::Instant::now();
        let index = references_to(Path::new(&root), &name);
        let elapsed = started.elapsed();
        eprintln!(
            "references_to({name}) -> {} hits, {} scanned, {} skipped, status {:?} in {elapsed:?}",
            index.references.len(),
            index.modules_scanned,
            index.modules_skipped,
            index.status,
        );
    }

    #[test]
    fn fresh_source_is_not_flagged() {
        // Index written after the source (stage writes index then source, but
        // we re-touch the index last to be unambiguous): no stale entry.
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        std::thread::sleep(Duration::from_millis(20));
        let index_file = project.path().join(BUILD_LIB_REL).join("Demo/A.ilean");
        fs::write(&index_file, fixture("demo_a.ilean")).unwrap();

        let index = references_to(project.path(), "Demo.A.foo");
        assert!(index.stale_sources.is_empty());
    }
}
