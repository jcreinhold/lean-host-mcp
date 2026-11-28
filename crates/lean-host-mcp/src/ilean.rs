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
//! fully-qualified name and returns a [`ReferenceIndex`] of hits grouped by
//! the module whose index recorded them (LSP range + def/ref kind).
//! Everything else — file enumeration, the compact-array JSON codecs and the
//! version gate — is hidden. The raw JSON types are private; callers never
//! see them. A module's source file is Lake's to name: callers ask
//! `ProjectBroker::module_sources`, since a library's `srcDir` decides it.
//!
//! A missing index is reported as **data**, not an error: a project that was
//! never built yields [`IndexStatus::NotBuilt`]. A single malformed
//! or unreadable `.ilean` is skipped and counted, never fatal. The consumer
//! (`find_references`) maps those signals onto the response envelope. Callers
//! hold the project's read turn, so no build writes an index while it is read.
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

/// A single reference to the queried name, in the source of the module whose
/// index recorded it.
///
/// Coordinates are 0-based LSP line/column, carried straight from the index.
/// Maps directly onto `find_references`'s `ReferenceHit`.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceLocation {
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
    /// Each module whose index recorded a hit, in walk order.
    pub modules: Vec<ModuleHits>,
    /// `.ilean` files parsed successfully.
    pub modules_scanned: usize,
    /// `.ilean` files skipped because they were unreadable, malformed, or an
    /// unsupported version. One bad file does not sink the query.
    pub modules_skipped: usize,
}

/// The hits one module's index recorded.
#[derive(Debug, Clone)]
pub(crate) struct ModuleHits {
    /// The module, as its index names it.
    pub module: String,
    /// The `.ilean` file that recorded the hits.
    pub index: PathBuf,
    pub references: Vec<ReferenceLocation>,
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
/// The scan reads every `.ilean` under the build tree. An index names its
/// module, not that module's source file, so callers that need the file ask
/// Lake for the modules in [`ReferenceIndex::modules`].
///
/// Infallible at the top level: a missing build directory yields
/// [`IndexStatus::NotBuilt`]; an individual unreadable or malformed `.ilean`
/// is counted in [`ReferenceIndex::modules_skipped`] and skipped. Never panics.
pub(crate) fn references_to(project_root: &Path, name: &str) -> ReferenceIndex {
    let build_dir = project_root.join(BUILD_LIB_REL);
    let mut found = ReferenceIndex {
        status: IndexStatus::NotBuilt,
        modules: Vec::new(),
        modules_scanned: 0,
        modules_skipped: 0,
    };
    if !build_dir.is_dir() {
        return found;
    }
    found.status = IndexStatus::Present;

    let matcher = KeyMatcher::new(name);
    for entry in WalkDir::new(&build_dir).into_iter().filter_map(std::result::Result::ok) {
        let index_path = entry.path();
        if !entry.file_type().is_file() || index_path.extension().is_none_or(|ext| ext != "ilean") {
            continue;
        }
        let Ok(module) = load_references(index_path, &matcher) else {
            found.modules_skipped = found.modules_skipped.saturating_add(1);
            continue;
        };
        found.modules_scanned = found.modules_scanned.saturating_add(1);
        if module.hits.is_empty() {
            continue;
        }
        found.modules.push(ModuleHits {
            module: module.module,
            index: index_path.to_path_buf(),
            references: module
                .hits
                .iter()
                .map(|(kind, location)| location_hit(location, *kind))
                .collect(),
        });
    }
    found
}

/// The declarations one module's index records, read from the index at the
/// path Lake's build layout gives that module.
pub(crate) fn declarations_in_module(project_root: &Path, module: &str) -> ModuleDeclarationIndex {
    let build_dir = project_root.join(BUILD_LIB_REL);
    let index = module_to_index(project_root, module);
    if !build_dir.is_dir() {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ProjectNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
        };
    }
    if !index.is_file() {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ModuleNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
        };
    }
    let Ok(raw) = load_declarations(&index) else {
        return ModuleDeclarationIndex {
            status: ModuleDeclarationIndexStatus::ModuleNotBuilt,
            module: module.to_owned(),
            index,
            declarations: Vec::new(),
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
    ModuleDeclarationIndex {
        status: ModuleDeclarationIndexStatus::Present,
        module: raw.module,
        index,
        declarations,
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
/// As [`load_references`].
fn load_declarations(path: &Path) -> Result<ModuleDeclarations, IleanError> {
    let bytes = read_bytes(path)?;
    let doc: ModuleDeclarations =
        serde_json::from_slice(&bytes).map_err(|source| classify_parse_failure(path, &bytes, source))?;
    match version_verdict(path, doc.version) {
        Some(error) => Err(error),
        None => Ok(doc),
    }
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

/// Project a raw index location onto a [`ReferenceLocation`].
const fn location_hit(location: &LocationRaw, kind: RefKind) -> ReferenceLocation {
    ReferenceLocation {
        start_line: location.start_line,
        start_column: location.start_column,
        end_line: location.end_line,
        end_column: location.end_column,
        kind,
    }
}

/// True when `source` changed after `index` was written, so the index may
/// record old positions. Best-effort: a stat failure on either side counts as
/// "not newer".
pub(crate) fn source_newer_than_index(source: &Path, index: &Path) -> bool {
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

    /// The hits of `kind` that `module`'s index recorded.
    fn hit(index: &ReferenceIndex, module: &str, kind: RefKind) -> Vec<(u32, u32, u32, u32)> {
        index
            .modules
            .iter()
            .filter(|hits| hits.module == module)
            .flat_map(|hits| &hits.references)
            .filter(|reference| reference.kind == kind)
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

    /// Every hit, from every module.
    fn every_hit(index: &ReferenceIndex) -> impl Iterator<Item = &ReferenceLocation> {
        index.modules.iter().flat_map(|hits| &hits.references)
    }

    #[test]
    fn resolves_def_and_usages_across_modules() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");

        assert_eq!(index.status, IndexStatus::Present);
        assert_eq!(index.modules_scanned, 2);
        assert_eq!(index.modules_skipped, 0);

        // The definition is in Demo.A.
        assert_eq!(hit(&index, "Demo.A", RefKind::Def), vec![(3, 4, 3, 7)]);
        assert!(hit(&index, "Demo.A", RefKind::Ref).is_empty());

        // Both usages are in Demo.B (one carries a 5th parentDecl element,
        // which must be parsed and discarded).
        assert!(hit(&index, "Demo.B", RefKind::Def).is_empty());
        let mut b_refs = hit(&index, "Demo.B", RefKind::Ref);
        b_refs.sort_unstable();
        assert_eq!(b_refs, vec![(5, 2, 5, 5), (6, 8, 6, 11)]);

        assert_eq!(every_hit(&index).count(), 3);

        // Each group names the index that recorded it, which is where a
        // freshness check looks.
        let build = project.path().join(BUILD_LIB_REL);
        let mut groups: Vec<_> = index
            .modules
            .iter()
            .map(|hits| (hits.module.as_str(), hits.index.clone()))
            .collect();
        groups.sort();
        assert_eq!(
            groups,
            vec![
                ("Demo.A", build.join("Demo/A.ilean")),
                ("Demo.B", build.join("Demo/B.ilean"))
            ]
        );
    }

    #[test]
    fn ignores_fvar_keys_and_mismatched_consts() {
        // demo_b.ilean carries an fvar key whose id is "Demo.A.foo" and a const
        // usage of "Init.Nat" — neither must surface for the queried name.
        let project = stage(&[("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");

        // Only the two const usages of Demo.A.foo, none from the fvar entry.
        assert_eq!(every_hit(&index).count(), 2);
        assert!(every_hit(&index).all(|reference| reference.kind == RefKind::Ref));
    }

    #[test]
    fn a_module_without_hits_forms_no_group() {
        let project = stage(&[("Demo.A", "demo_a.ilean"), ("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Nat");

        assert_eq!(index.modules_scanned, 2);
        let modules: Vec<_> = index.modules.iter().map(|hits| hits.module.as_str()).collect();
        assert_eq!(modules, vec!["Demo.A"], "only Demo.A uses Nat");
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
        let index = references_to(project.path(), "Demo.A.foo");
        assert_eq!(index.status, IndexStatus::Present);
        assert_eq!(index.modules_scanned, 1);
        assert_eq!(index.modules_skipped, 1);
        assert_eq!(hit(&index, "Demo.A", RefKind::Def), vec![(3, 4, 3, 7)]);
    }

    #[test]
    fn null_definition_yields_only_usages() {
        // Demo.B's entry for Demo.A.foo has a null definition.
        let project = stage(&[("Demo.B", "demo_b.ilean")]);
        let index = references_to(project.path(), "Demo.A.foo");
        assert!(every_hit(&index).all(|reference| reference.kind == RefKind::Ref));
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
        assert!(index.modules.is_empty());
        assert_eq!(index.modules_scanned, 0);
    }

    #[test]
    fn a_source_edited_after_its_index_is_newer() {
        // A short sleep between writes guarantees distinct mtimes whatever the
        // file system's timestamp resolution.
        let project = stage(&[("Demo.A", "demo_a.ilean")]);
        let source = project.path().join("Demo/A.lean");
        let index = project.path().join(BUILD_LIB_REL).join("Demo/A.ilean");

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&index, fixture("demo_a.ilean")).unwrap();
        assert!(!source_newer_than_index(&source, &index), "rebuilt after the edit");

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&source, "-- edited after build\n").unwrap();
        assert!(source_newer_than_index(&source, &index), "edited after the build");

        assert!(
            !source_newer_than_index(&project.path().join("Demo/Gone.lean"), &index),
            "a file that cannot be read counts as not newer"
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
            every_hit(&index).count(),
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

        let index = references_to(project.path(), "Demo.V.foo");
        assert_eq!(index.modules_scanned, 1);
        assert_eq!(index.modules_skipped, 0);
        assert_eq!(hit(&index, "Demo.V", RefKind::Def), vec![(3, 4, 3, 7)]);

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
        let index = references_to(project.path(), "Demo.A.foo");
        assert_eq!(index.modules_skipped, 1, "only Demo.B's garbled references");
        assert_eq!(hit(&index, "Demo.A", RefKind::Def), vec![(3, 4, 3, 7)]);

        // ...and garbled `references` must not cost a declaration.
        let declarations = declarations_in_module(project.path(), "Demo.B");
        assert_eq!(declarations.status, ModuleDeclarationIndexStatus::Present);
        assert_eq!(declarations.declarations.len(), 1);
        assert_eq!(declarations.declarations.first().unwrap().name, "Demo.B.bar");
    }
}
