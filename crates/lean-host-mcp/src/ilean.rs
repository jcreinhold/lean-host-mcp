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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, DeserializeSeed as _, Deserializer, SeqAccess, Visitor};
use walkdir::WalkDir;

/// Only this `.ilean` format version is understood. A different version is
/// rejected rather than parsed into a silent wrong answer.
const SUPPORTED_VERSION: u64 = 5;

/// Project-relative path to the directory holding the project's own module
/// indices. Dependency indices live under separate `.lake/packages/*/` trees,
/// so a recursive walk of this directory yields exactly the project's modules.
use crate::lake_meta::BUILD_LIB_REL;

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
/// `files`, when given, restricts the answer to references *occurring in* those
/// source files. Because an `.ilean` records only the references occurring in
/// its own module's source, that restriction is also a restriction on which
/// indices can contribute — so the scan reads just those, instead of reading the
/// whole project and discarding the rest. When the subset cannot be mapped onto
/// index paths with certainty the full walk runs anyway, so a narrowed request
/// never returns less than an unnarrowed one.
///
/// Infallible at the top level: a missing build directory yields
/// [`IndexStatus::NotBuilt`]; an individual unreadable or malformed `.ilean`
/// is counted in [`ReferenceIndex::modules_skipped`] and skipped. Never panics.
pub(crate) fn references_to(project_root: &Path, name: &str, files: Option<&[PathBuf]>) -> ReferenceIndex {
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

    let matcher = KeyMatcher::new(name);
    let mut scan = ReferenceScan::default();

    match files.and_then(|files| indices_for(project_root, &build_dir, files)) {
        Some(indices) => {
            for index_path in &indices {
                scan.visit(project_root, index_path, &matcher);
            }
        }
        None => {
            for entry in WalkDir::new(&build_dir).into_iter().filter_map(std::result::Result::ok) {
                let index_path = entry.path();
                if !entry.file_type().is_file() || index_path.extension().is_none_or(|ext| ext != "ilean") {
                    continue;
                }
                scan.visit(project_root, index_path, &matcher);
            }
        }
    }

    ReferenceIndex {
        status: IndexStatus::Present,
        stale_sources: collect_stale_sources(&scan.contributing),
        references: scan.references,
        modules_scanned: scan.modules_scanned,
        modules_skipped: scan.modules_skipped,
    }
}

/// Running state of one project-scope scan, so the whole-tree walk and an
/// explicit index list share a single per-file body.
#[derive(Default)]
struct ReferenceScan {
    references: Vec<ReferenceLocation>,
    modules_scanned: usize,
    modules_skipped: usize,
    /// (source path, index path) for each module that produced a hit; stale
    /// detection only touches these, keeping the stat work off the hot path.
    contributing: Vec<(PathBuf, PathBuf)>,
}

impl ReferenceScan {
    fn visit(&mut self, project_root: &Path, index_path: &Path, matcher: &KeyMatcher<'_>) {
        let Ok(module) = load_references(index_path, matcher) else {
            self.modules_skipped = self.modules_skipped.saturating_add(1);
            return;
        };
        self.modules_scanned = self.modules_scanned.saturating_add(1);

        if module.hits.is_empty() {
            return;
        }
        let source = module_to_source(project_root, &module.module);
        for (kind, location) in &module.hits {
            self.references.push(location_hit(&source, location, *kind));
        }
        self.contributing.push((source, index_path.to_path_buf()));
    }
}

/// The exact index paths that can contribute references occurring in `files`,
/// or `None` when the subset cannot be inverted with certainty.
///
/// [`module_to_source`] maps a dotted module onto `<root>/A/B.lean`, so the
/// inverse of `<root>/A/B.lean` is the index at `<build>/A/B.ilean` — the same
/// path math as [`module_to_index`], without needing the module name. Two
/// conditions make that inverse exact, and failing either falls back to the full
/// walk rather than guessing:
///
/// - **No dotted path component.** `module_to_source` splits on `.`, so a
///   component containing one is not in its image and the mapping would be
///   inventing a module.
/// - **The computed index exists.** Lake mangles some module names on disk
///   (module `«kan-lint-style»` indexes as `kan-lint-style.ilean`), so a missing
///   file means the real index lives under a name this mapping cannot predict —
///   not that the module has no references.
fn indices_for(project_root: &Path, build_dir: &Path, files: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut indices = Vec::with_capacity(files.len());
    for file in files {
        let absolute = if file.is_absolute() {
            Cow::Borrowed(file.as_path())
        } else {
            Cow::Owned(project_root.join(file))
        };
        let relative = absolute.strip_prefix(project_root).ok()?;
        if relative.extension()? != "lean" {
            return None;
        }
        // Drop the extension before the dot check so `A/B.lean` passes while
        // `A/B.C.lean` — which no `split('.')` can produce — does not.
        let plain_components = relative
            .with_extension("")
            .components()
            .all(|component| match component {
                std::path::Component::Normal(part) => part.to_str().is_some_and(|part| !part.contains('.')),
                std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::CurDir
                | std::path::Component::ParentDir => false,
            });
        if !plain_components {
            return None;
        }
        let index = build_dir.join(relative).with_extension("ilean");
        if !index.is_file() {
            return None;
        }
        indices.push(index);
    }
    Some(indices)
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
    let Ok(raw) = load_declarations(&index) else {
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

fn read_bytes(path: &Path) -> Result<Vec<u8>, IleanError> {
    std::fs::read(path).map_err(|source| IleanError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Gate a successfully parsed document on the `version` it carried.
///
/// The gate runs *after* the parse rather than before it because `version` is
/// the **last** key in a real `.ilean`: Lean's `Json` object is an `RBNode`, so
/// `Json.compress` emits keys sorted, putting `version` after `references`. A
/// "probe the version first" pass therefore reads the whole document anyway —
/// it was never the cheap prefix read it looked like, and running it doubled
/// the cost of every file.
fn version_verdict(path: &Path, version: Option<u64>) -> Option<IleanError> {
    match version {
        Some(SUPPORTED_VERSION) => None,
        Some(found) => Some(IleanError::UnsupportedVersion {
            path: path.to_path_buf(),
            found,
        }),
        // Byte-identical to what the derive produced when `version` was a
        // required field on the probe.
        None => Some(IleanError::Json {
            path: path.to_path_buf(),
            source: <serde_json::Error as de::Error>::missing_field("version"),
        }),
    }
}

/// Decide what a projection-parse failure means.
///
/// The old reader gated on `version` before the real parse, so a future-version
/// file whose *shape* this reader cannot read reported `UnsupportedVersion`
/// rather than `Json`. Re-probing only on the failure path preserves that
/// verdict without paying a second full pass on every good file.
fn classify_parse_failure(path: &Path, bytes: &[u8], source: serde_json::Error) -> IleanError {
    match serde_json::from_slice::<VersionProbe>(bytes) {
        Ok(probe) if probe.version != SUPPORTED_VERSION => IleanError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: probe.version,
        },
        Ok(_) | Err(_) => IleanError::Json {
            path: path.to_path_buf(),
            source,
        },
    }
}

/// Read, parse, and version-gate one `.ilean` file, keeping only the hits for
/// the name `matcher` describes.
///
/// # Errors
///
/// [`IleanError::Io`] when the file cannot be read, [`IleanError::Json`] on a
/// parse failure or a missing `version`, and [`IleanError::UnsupportedVersion`]
/// when `version` is not [`SUPPORTED_VERSION`]. A document that parses but
/// carries the wrong version is discarded whole — including any hits already
/// collected from it — so an unknown format never yields a wrong answer.
fn load_references(path: &Path, matcher: &KeyMatcher<'_>) -> Result<ModuleReferences, IleanError> {
    let bytes = read_bytes(path)?;
    let doc = parse_references(&bytes, matcher).map_err(|source| classify_parse_failure(path, &bytes, source))?;
    match version_verdict(path, doc.version) {
        Some(error) => Err(error),
        None => Ok(doc),
    }
}

/// Drive the seeded parse. `serde_json` has no `from_slice_seed`, so this
/// reproduces what `from_slice` does — including `end()`, which is what rejects
/// trailing content after the document.
fn parse_references(bytes: &[u8], matcher: &KeyMatcher<'_>) -> Result<ModuleReferences, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let parsed = ModuleReferencesSeed { matcher }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(parsed)
}

/// Read, parse, and version-gate one `.ilean` file for the declaration query.
///
/// Models `decls` and nothing else: serde skips `references` — ~97% of a real
/// file's bytes — with `IgnoredAny`, a non-recursive byte scan that allocates
/// nothing, instead of materializing a `HashMap` this caller never reads.
///
/// # Errors
///
/// As [`load`].
fn load_declarations(path: &Path) -> Result<ModuleDeclarations, IleanError> {
    let bytes = read_bytes(path)?;
    let doc: ModuleDeclarations =
        serde_json::from_slice(&bytes).map_err(|source| classify_parse_failure(path, &bytes, source))?;
    match version_verdict(path, doc.version) {
        Some(error) => Err(error),
        None => Ok(doc),
    }
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

/// Precomputed state for testing one `references` map key against the query.
///
/// A key is itself a compressed-JSON object — the externally-tagged `RefIdent`:
/// `{"c":{"m":definingModule,"n":ident}}` for a global name, or
/// `{"f":{"m":module,"i":id}}` for a local fvar. Only `const` keys participate
/// in find-references; fvars and malformed keys are ignored.
///
/// Two stages, because the exact test is a JSON parse and almost every key
/// fails: a cheap necessary condition first, the exact one only for survivors.
struct KeyMatcher<'a> {
    name: &'a str,
    /// The substring gate, when the name is its own JSON encoding.
    ///
    /// `None` when `name` contains a byte JSON must escape, because then it is
    /// *not* a substring of its encoded form and the gate would be unsound —
    /// every key goes straight to the exact parse instead. (The old
    /// `key.contains(name)` gate had no such guard, so it silently missed names
    /// containing a quote or backslash.)
    gate: Option<&'a str>,
}

impl<'a> KeyMatcher<'a> {
    fn new(name: &'a str) -> Self {
        let needs_escape = name.bytes().any(|byte| byte == b'"' || byte == b'\\' || byte < 0x20);
        Self {
            name,
            gate: (!needs_escape).then_some(name),
        }
    }

    /// True when `key` is a `const` `RefIdent` whose identifier equals the
    /// query. Allocates nothing on the overwhelmingly common non-matching path,
    /// and borrows the identifier out of `key` on the matching one.
    fn matches(&self, key: &str) -> bool {
        if self.gate.is_some_and(|needle| !key.contains(needle)) {
            return false;
        }
        match serde_json::from_str::<RefIdentKey<'_>>(key) {
            Ok(RefIdentKey::Const { n }) => n.as_ref() == self.name,
            Ok(RefIdentKey::Fvar {}) | Err(_) => false,
        }
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

/// Cold-path classifier: reads `version` and ignores every other field.
///
/// This is **not** a cheap prefix read — `version` is the last key in a real
/// `.ilean`, so this walks the whole document. It runs only when a projection
/// parse has already failed, to decide whether the failure was an unsupported
/// version or a genuinely malformed file. See [`classify_parse_failure`].
#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
}

/// The compressed-JSON `RefIdent` used as a `references` map key, parsed from
/// its externally-tagged form. Only the const identifier `n` is read; the
/// defining module and the fvar payload are intentionally ignored.
#[derive(Deserialize)]
enum RefIdentKey<'a> {
    #[serde(rename = "c")]
    Const {
        /// Borrowed out of the key unless the name carries a JSON escape, in
        /// which case serde unescapes into an owned string.
        #[serde(borrow)]
        n: Cow<'a, str>,
    },
    #[serde(rename = "f")]
    Fvar {},
}

/// One module's contribution to a reference query, and nothing else.
///
/// The point of the type is what it *lacks*: no map of every key in the file,
/// no locations for names nobody asked about. `hits` is O(matching locations),
/// never O(entries in the file).
struct ModuleReferences {
    module: String,
    version: Option<u64>,
    hits: Vec<(RefKind, LocationRaw)>,
}

/// Top-level `.ilean` field names, resolved without allocating.
enum DocField {
    Module,
    References,
    Version,
    /// `decls`, `directImports`, or anything a later schema adds.
    Other,
}

impl<'de> Deserialize<'de> for DocField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DocFieldVisitor;

        impl Visitor<'_> for DocFieldVisitor {
            type Value = DocField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an .ilean top-level field name")
            }

            fn visit_str<E>(self, value: &str) -> Result<DocField, E>
            where
                E: de::Error,
            {
                Ok(match value {
                    "module" => DocField::Module,
                    "references" => DocField::References,
                    "version" => DocField::Version,
                    _ => DocField::Other,
                })
            }
        }

        deserializer.deserialize_str(DocFieldVisitor)
    }
}

/// Deserializes an `.ilean` document, keeping only the queried name's hits.
///
/// The query name is a runtime value, so a plain `Deserialize` impl cannot see
/// it — hence a seed. This is the whole optimization: the reader still walks
/// every byte of `references` (it must, to find the end of the map), but it
/// builds a value only for entries that match.
struct ModuleReferencesSeed<'m, 'a> {
    matcher: &'m KeyMatcher<'a>,
}

impl<'de> de::DeserializeSeed<'de> for ModuleReferencesSeed<'_, '_> {
    type Value = ModuleReferences;

    fn deserialize<D>(self, deserializer: D) -> Result<ModuleReferences, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DocVisitor<'m, 'a> {
            matcher: &'m KeyMatcher<'a>,
        }

        impl<'de> Visitor<'de> for DocVisitor<'_, '_> {
            type Value = ModuleReferences;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an .ilean document")
            }

            fn visit_map<A>(self, mut map: A) -> Result<ModuleReferences, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut module: Option<String> = None;
                let mut version: Option<u64> = None;
                let mut hits: Option<Vec<(RefKind, LocationRaw)>> = None;
                while let Some(field) = map.next_key::<DocField>()? {
                    match field {
                        DocField::Module => {
                            if module.is_some() {
                                return Err(de::Error::duplicate_field("module"));
                            }
                            module = Some(map.next_value()?);
                        }
                        DocField::Version => {
                            if version.is_some() {
                                return Err(de::Error::duplicate_field("version"));
                            }
                            version = Some(map.next_value()?);
                        }
                        DocField::References => {
                            if hits.is_some() {
                                return Err(de::Error::duplicate_field("references"));
                            }
                            hits = Some(map.next_value_seed(MatchingEntriesSeed { matcher: self.matcher })?);
                        }
                        DocField::Other => {
                            map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(ModuleReferences {
                    module: module.ok_or_else(|| de::Error::missing_field("module"))?,
                    version,
                    hits: hits.ok_or_else(|| de::Error::missing_field("references"))?,
                })
            }
        }

        deserializer.deserialize_map(DocVisitor { matcher: self.matcher })
    }
}

/// Deserializes the `references` map, keeping only matching entries.
struct MatchingEntriesSeed<'m, 'a> {
    matcher: &'m KeyMatcher<'a>,
}

impl<'de> de::DeserializeSeed<'de> for MatchingEntriesSeed<'_, '_> {
    type Value = Vec<(RefKind, LocationRaw)>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct EntriesVisitor<'m, 'a> {
            matcher: &'m KeyMatcher<'a>,
        }

        impl<'de> Visitor<'de> for EntriesVisitor<'_, '_> {
            type Value = Vec<(RefKind, LocationRaw)>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an .ilean references map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                // No capacity hint: a hit is rare, and pre-sizing to the map
                // would reintroduce the allocation this design removes.
                let mut hits = Vec::new();
                while let Some(matched) = map.next_key_seed(KeyMatchSeed { matcher: self.matcher })? {
                    if matched {
                        let info: RefInfoRaw = map.next_value()?;
                        if let Some(definition) = info.definition {
                            hits.push((RefKind::Def, definition));
                        }
                        hits.extend(info.usages.into_iter().map(|usage| (RefKind::Ref, usage)));
                    } else {
                        // `ignore_value`: a non-recursive byte scan that
                        // allocates nothing.
                        map.next_value::<de::IgnoredAny>()?;
                    }
                }
                Ok(hits)
            }
        }

        deserializer.deserialize_map(EntriesVisitor { matcher: self.matcher })
    }
}

/// Answers "is this key the queried name?" without keeping the key.
///
/// `serde_json` hands map keys to the visitor as a transient `&str` in a reused
/// scratch buffer, so consuming it and returning a `bool` costs no allocation —
/// which is what makes skipping ~400k keys per project nearly free.
struct KeyMatchSeed<'m, 'a> {
    matcher: &'m KeyMatcher<'a>,
}

impl<'de> de::DeserializeSeed<'de> for KeyMatchSeed<'_, '_> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> Result<bool, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeyMatchVisitor<'m, 'a> {
            matcher: &'m KeyMatcher<'a>,
        }

        impl Visitor<'_> for KeyMatchVisitor<'_, '_> {
            type Value = bool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a compressed-JSON RefIdent map key")
            }

            fn visit_str<E>(self, value: &str) -> Result<bool, E>
            where
                E: de::Error,
            {
                Ok(self.matcher.matches(value))
            }
        }

        deserializer.deserialize_str(KeyMatchVisitor { matcher: self.matcher })
    }
}

/// The `.ilean` projection the **declaration** query consumes.
///
/// The mirror image of [`ModuleReferences`]: `references` is not modeled, so
/// serde skips it. That is the whole reason there are two document shapes
/// rather than one shared type — the two entry points read disjoint halves of
/// the file, and a shared shape makes each pay for the other's. `decls` is ~3%
/// of a real file's bytes; `references` is ~97%.
#[derive(Deserialize)]
struct ModuleDeclarations {
    /// Dotted module name, e.g. `KanProofs.Foo.Bar`.
    module: String,
    #[serde(default)]
    version: Option<u64>,
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
        let bodies: Vec<(&str, String)> = modules
            .iter()
            .map(|(module, fixture_name)| (*module, fixture(fixture_name)))
            .collect();
        stage_bodies(&bodies)
    }

    /// Same on-disk layout as [`stage`], with the `.ilean` body given inline.
    /// The shape probes below are *about* the body, so keeping it beside the
    /// assertion says more than a committed file named after the property.
    fn stage_bodies(modules: &[(&str, String)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join(BUILD_LIB_REL);
        for (module, body) in modules {
            let relative: PathBuf = module.split('.').collect();

            let index = build.join(&relative).with_extension("ilean");
            fs::create_dir_all(index.parent().unwrap()).unwrap();
            fs::write(&index, body).unwrap();

            let source = tmp.path().join(&relative).with_extension("lean");
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, "-- source stub\n").unwrap();
        }
        tmp
    }

    /// A `references` key as Lean writes it: compressed JSON, itself a string.
    /// Built through `serde_json` rather than `format!` so the escaping in the
    /// escaped-name case is the encoder's, not the test author's.
    fn const_key(module: &str, name: &str) -> String {
        serde_json::json!({ "c": { "m": module, "n": name } }).to_string()
    }

    fn fvar_key(module: &str, id: &str) -> String {
        serde_json::json!({ "f": { "m": module, "i": id } }).to_string()
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
        let index = references_to(project.path(), "Demo.A.foo", None);

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
        let index = references_to(project.path(), "Demo.A.foo", None);

        // Only the two const usages of Demo.A.foo, none from the fvar entry.
        assert_eq!(index.references.len(), 2);
        assert!(index.references.iter().all(|reference| reference.kind == RefKind::Ref));
    }

    #[test]
    fn unknown_version_is_rejected_with_typed_error() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ilean/bad_version.ilean");
        match load_references(&path, &KeyMatcher::new("Demo.A.foo")) {
            Err(IleanError::UnsupportedVersion { found, .. }) => assert_eq!(found, 99),
            Err(other) => panic!("expected UnsupportedVersion, got error {other:?}"),
            Ok(_) => panic!("expected UnsupportedVersion, got a successful load"),
        }
    }

    #[test]
    fn malformed_file_is_skipped_not_fatal() {
        // the reader surfaces a typed Json error directly...
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ilean/malformed.ilean");
        assert!(matches!(
            load_references(&path, &KeyMatcher::new("Demo.A.foo")),
            Err(IleanError::Json { .. })
        ));

        // ...and a project query counts it as skipped while still resolving the
        // good module alongside it.
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.Bad", "malformed.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo", None);
        assert_eq!(index.status, IndexStatus::Present);
        assert_eq!(index.modules_scanned, 1);
        assert_eq!(index.modules_skipped, 1);
        assert_eq!(hit(&index, "Demo/A.lean", RefKind::Def), vec![(3, 4, 3, 7)]);
    }

    #[test]
    fn null_definition_yields_only_usages() {
        // Demo.B's entry for Demo.A.foo has a null definition.
        let project = stage(&[("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo", None);
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

        let index = references_to(tmp.path(), "Demo.A.foo", None);
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

        let index = references_to(project.path(), "Demo.A.foo", None);
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
        let index = references_to(Path::new(&root), &name, None);
        let elapsed = started.elapsed();
        eprintln!(
            "references_to({name}) -> {} hits, {} scanned, {} skipped, status {:?} in {elapsed:?}",
            index.references.len(),
            index.modules_scanned,
            index.modules_skipped,
            index.status,
        );
    }

    /// Lean's `Json` object is an `RBNode`, so `Json.compress` emits keys
    /// sorted and `version` lands *last* — after the megabytes of
    /// `references` it was once believed to gate. Every other fixture here puts
    /// it first and so resembles no real `.ilean`; this one is shaped like the
    /// files on disk, and it is what fails if anyone re-optimizes the version
    /// check back into a cheap prefix probe.
    #[test]
    fn version_may_be_the_last_field() {
        let body = format!(
            r#"{{"decls":{{"Demo.V.foo":[3,4,3,7,3,4,3,7]}},"directImports":[],"module":"Demo.V","references":{{{key}:{{"definition":[3,4,3,7],"usages":[]}}}},"version":5}}"#,
            key = serde_json::to_string(&const_key("Demo.V", "Demo.V.foo")).unwrap(),
        );
        let project = stage_bodies(&[("Demo.V", body)]);

        let index = references_to(project.path(), "Demo.V.foo", None);
        assert_eq!(index.modules_scanned, 1);
        assert_eq!(index.modules_skipped, 0);
        assert_eq!(hit(&index, "Demo/V.lean", RefKind::Def), vec![(3, 4, 3, 7)]);

        // The declaration reader reaches `version` past a trailing key too.
        let declarations = declarations_in_module(project.path(), "Demo.V");
        assert_eq!(declarations.status, ModuleDeclarationIndexStatus::Present);
        assert_eq!(declarations.declarations.len(), 1);
    }

    /// A future `.ilean` whose shape this reader cannot parse must still report
    /// "unsupported version", not "malformed JSON" — the version is the
    /// actionable fact. That verdict now comes from a re-probe on the failure
    /// path, and without this test the re-probe reads as dead code.
    #[test]
    fn future_version_with_unreadable_shape_is_unsupported() {
        let project = stage_bodies(&[(
            "Demo.F",
            r#"{"decls":{},"module":"Demo.F","references":42,"version":99}"#.to_owned(),
        )]);
        let path = project.path().join(BUILD_LIB_REL).join("Demo/F.ilean");

        match load_references(&path, &KeyMatcher::new("Demo.F.foo")) {
            Err(IleanError::UnsupportedVersion { found, .. }) => assert_eq!(found, 99),
            Err(other) => panic!("expected UnsupportedVersion, got error {other:?}"),
            Ok(_) => panic!("expected UnsupportedVersion, got a successful load"),
        }
    }

    #[test]
    fn missing_version_is_a_json_error() {
        let project = stage_bodies(&[("Demo.N", r#"{"decls":{},"module":"Demo.N","references":{}}"#.to_owned())]);
        let path = project.path().join(BUILD_LIB_REL).join("Demo/N.ilean");

        match load_references(&path, &KeyMatcher::new("Demo.N.foo")) {
            Err(IleanError::Json { source, .. }) => {
                assert!(
                    source.to_string().contains("version"),
                    "the error must name the missing field, got {source}"
                );
            }
            Err(other) => panic!("expected a Json error, got {other:?}"),
            Ok(_) => panic!("expected a Json error, got a successful load"),
        }
    }

    /// The substring pre-gate is a *necessary* condition only, so every case
    /// where it passes but the key does not match must still be rejected by the
    /// exact parse — and every case where the gate must be disabled has to
    /// reach that parse at all.
    #[test]
    fn key_matcher_table() {
        let cases: &[(&str, String, bool)] = &[
            // Exact const hit.
            ("Demo.A.foo", const_key("Demo.A", "Demo.A.foo"), true),
            // The name appears as the *module* component, not the name: the
            // pre-gate passes on the raw key text and the parse must reject.
            ("Demo.A.foo", const_key("Demo.A.foo", "Demo.A.foo.aux"), false),
            // Strict substring of a longer name.
            ("Demo.A.foo", const_key("Demo.A", "Demo.A.foobar"), false),
            // An fvar whose id happens to equal the queried name.
            ("Demo.A.foo", fvar_key("Demo.B", "Demo.A.foo"), false),
            // Guillemet module names, as Lake emits for hyphenated packages.
            (
                "«kan-lint-style».run",
                const_key("«kan-lint-style»", "«kan-lint-style».run"),
                true,
            ),
            // Private-name mangling.
            (
                "_private.Demo.A.0.Demo.A.helper",
                const_key("Demo.A", "_private.Demo.A.0.Demo.A.helper"),
                true,
            ),
            // A name JSON must escape: the pre-gate is unsound here (the name is
            // not a substring of its encoded form) and so must be disabled,
            // leaving the exact parse — over an owned, unescaped `Cow` — to
            // decide both the hit and the miss.
            ("Demo.\"odd\"", const_key("Demo", "Demo.\"odd\""), true),
            ("Demo.\"odd\"", const_key("Demo", "Demo.other"), false),
        ];

        for (name, key, expected) in cases {
            let matcher = KeyMatcher::new(name);
            assert_eq!(
                matcher.matches(key),
                *expected,
                "KeyMatcher::new({name:?}).matches({key:?})"
            );
        }

        assert!(KeyMatcher::new("Demo.A.foo").gate.is_some());
        assert!(
            KeyMatcher::new("Demo.\"odd\"").gate.is_none(),
            "a name JSON must escape cannot be pre-gated by substring"
        );
    }

    /// The two entry points read disjoint halves of the document — `references`
    /// is ~97% of the bytes and `decls` ~3% — so neither may be able to break
    /// the other. Both halves of this failed while one shared type modeled the
    /// whole file.
    #[test]
    fn projections_are_independent() {
        let good_references = format!(
            r#"{key}:{{"definition":[3,4,3,7],"usages":[]}}"#,
            key = serde_json::to_string(&const_key("Demo.A", "Demo.A.foo")).unwrap(),
        );
        let project = stage_bodies(&[
            (
                "Demo.A",
                format!(r#"{{"decls":42,"module":"Demo.A","references":{{{good_references}}},"version":5}}"#),
            ),
            (
                "Demo.B",
                r#"{"decls":{"Demo.B.bar":[9,0,9,3,9,0,9,3]},"module":"Demo.B","references":42,"version":5}"#
                    .to_owned(),
            ),
        ]);

        // Garbled `decls` must not cost a reference hit...
        let index = references_to(project.path(), "Demo.A.foo", None);
        assert_eq!(index.modules_skipped, 1, "only Demo.B's garbled references");
        assert_eq!(hit(&index, "Demo/A.lean", RefKind::Def), vec![(3, 4, 3, 7)]);

        // ...and garbled `references` must not cost a declaration.
        let declarations = declarations_in_module(project.path(), "Demo.B");
        assert_eq!(declarations.status, ModuleDeclarationIndexStatus::Present);
        assert_eq!(declarations.declarations.len(), 1);
        assert_eq!(declarations.declarations.first().unwrap().name, "Demo.B.bar");
    }

    /// Narrowing changes which files are *read*, never which hits exist: a
    /// restricted scan returns exactly the full walk's hits for those files.
    #[test]
    fn narrowing_agrees_with_the_full_walk() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        let full = references_to(project.path(), "Demo.A.foo", None);
        assert_eq!(full.modules_scanned, 2);

        for file in [PathBuf::from("Demo/B.lean"), project.path().join("Demo/B.lean")] {
            let narrowed = references_to(project.path(), "Demo.A.foo", Some(std::slice::from_ref(&file)));
            assert_eq!(narrowed.modules_scanned, 1, "read one index for {file:?}");
            assert_eq!(
                hit(&narrowed, "Demo/B.lean", RefKind::Ref),
                hit(&full, "Demo/B.lean", RefKind::Ref),
            );
            assert!(
                hit(&narrowed, "Demo/A.lean", RefKind::Def).is_empty(),
                "a narrowed scan never reads the other module"
            );
        }
    }

    /// A path this mapping cannot invert falls back to the whole walk. Guessing
    /// instead would silently drop hits, which is the failure the fallback
    /// exists to prevent.
    #[test]
    fn narrowing_falls_back_when_a_path_cannot_be_inverted() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        let build = project.path().join(BUILD_LIB_REL);

        let rejected = [
            // No `M.split('.')` produces a dotted component.
            PathBuf::from("Demo/A.B.lean"),
            // Not a Lean source at all.
            PathBuf::from("Demo/A.olean"),
            // Outside the project root.
            PathBuf::from("/elsewhere/Demo/A.lean"),
            // Escapes the root the long way.
            PathBuf::from("../Demo/A.lean"),
        ];
        for file in &rejected {
            assert!(
                indices_for(project.path(), &build, std::slice::from_ref(file)).is_none(),
                "{file:?} must not be inverted"
            );
            let index = references_to(project.path(), "Demo.A.foo", Some(std::slice::from_ref(file)));
            assert_eq!(index.modules_scanned, 2, "{file:?} must fall back to the full walk");
        }
    }

    /// Lake mangles some module names on disk (module `«kan-lint-style»`
    /// indexes as `kan-lint-style.ilean`), so a computed index that does not
    /// exist means the real one lives under a name this mapping cannot
    /// predict — never that the module has no references.
    #[test]
    fn narrowing_falls_back_when_the_computed_index_is_missing() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        fs::write(project.path().join("Demo/C.lean"), "-- no index of this name\n").unwrap();

        let files = [PathBuf::from("Demo/C.lean"), PathBuf::from("Demo/B.lean")];
        let index = references_to(project.path(), "Demo.A.foo", Some(&files));
        assert_eq!(
            index.modules_scanned, 2,
            "one un-invertible member falls the whole request back"
        );
        assert_eq!(hit(&index, "Demo/A.lean", RefKind::Def), vec![(3, 4, 3, 7)]);
    }

    #[test]
    fn fresh_source_is_not_flagged() {
        // Index written after the source (stage writes index then source, but
        // we re-touch the index last to be unambiguous): no stale entry.
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        std::thread::sleep(Duration::from_millis(20));
        let index_file = project.path().join(BUILD_LIB_REL).join("Demo/A.ilean");
        fs::write(&index_file, fixture("demo_a.ilean")).unwrap();

        let index = references_to(project.path(), "Demo.A.foo", None);
        assert!(index.stale_sources.is_empty());
    }
}
