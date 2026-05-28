//! Host-side Mathlib placement advice.
//!
//! This tool is intentionally policy-shaped: it walks Mathlib source files
//! under a bounded root and optionally samples existing semantic tools for the
//! one declaration or statement the caller selected. It never builds a global
//! semantic index and never mutates source files.

// Tool handlers consume owned requests so worker calls can cross async
// boundaries without borrow plumbing.
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::Result;
use crate::projections::DeclarationInspectionResult;
use crate::tools::declaration::{InspectDeclarationFields, InspectDeclarationRequest, inspect_declaration};
use crate::tools::proof_search::{SearchForProofRequest, search_for_proof};
use crate::tools::{ToolContext, freshness_for_meta, is_ignored_dir};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 25;
const MAX_FILES_SCANNED: usize = 2000;
const MAX_MATCHES_CONSIDERED: usize = 400;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MathlibPlacementRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub column: Option<u32>,
    #[serde(default)]
    pub statement: Option<String>,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub proposed_name: Option<String>,
    #[serde(default)]
    pub mathlib_root: Option<PathBuf>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlacementSourceHit {
    pub file: String,
    pub line: u32,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlacementSourceFacts {
    pub mathlib_root: String,
    pub files_scanned: usize,
    pub files_skipped: usize,
    pub matches_considered: usize,
    pub truncated: bool,
    pub source_based: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MathlibPlacementResult {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checked: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likely_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likely_file: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggested_imports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nearby_declarations: Vec<PlacementSourceHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub possible_duplicates: Vec<PlacementSourceHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub naming_examples: Vec<PlacementSourceHit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upstream_readiness_notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_facts: Option<Box<PlacementSourceFacts>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub semantic_facts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub source_based: bool,
    pub semantic_based: bool,
}

#[derive(Debug)]
struct MathlibRoot {
    path: PathBuf,
}

#[derive(Debug)]
struct SourceMatch {
    file: PathBuf,
    line: u32,
    text: String,
    reason: String,
}

#[derive(Debug, Default)]
struct SourcePlacementFacts {
    matches: Vec<SourceMatch>,
    files_scanned: usize,
    files_skipped: usize,
    truncated: bool,
}

/// Advise where a declaration belongs in a Mathlib-compatible source layout.
///
/// # Errors
///
/// Returns infrastructure failures from optional semantic sampling. Missing
/// Mathlib source roots and insufficient target input are structured results.
pub async fn mathlib_placement(
    ctx: &ToolContext,
    req: MathlibPlacementRequest,
) -> Result<Response<MathlibPlacementResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let freshness = freshness_for_meta(&meta);
    let Some(mathlib_root) = discover_mathlib_root(&req, meta.canonical_root.as_path()) else {
        let checked = checked_mathlib_roots(&req, meta.canonical_root.as_path());
        return Ok(Response::ok(
            MathlibPlacementResult {
                status: "missing_mathlib_root".to_owned(),
                message: None,
                checked,
                likely_namespace: None,
                likely_file: None,
                suggested_imports: Vec::new(),
                nearby_declarations: Vec::new(),
                possible_duplicates: Vec::new(),
                naming_examples: Vec::new(),
                upstream_readiness_notes: Vec::new(),
                source_facts: None,
                semantic_facts: Vec::new(),
                warnings: Vec::new(),
                source_based: true,
                semantic_based: false,
            },
            freshness,
        ));
    };

    let target = target_terms(&req);
    if target.is_empty() {
        return Ok(Response::ok(
            MathlibPlacementResult {
                status: "invalid_request".to_owned(),
                message: Some(
                    "mathlib_placement requires `name`, cursor input, `statement`, `concepts`, or `proposed_name`"
                        .to_owned(),
                ),
                checked: Vec::new(),
                likely_namespace: None,
                likely_file: None,
                suggested_imports: Vec::new(),
                nearby_declarations: Vec::new(),
                possible_duplicates: Vec::new(),
                naming_examples: Vec::new(),
                upstream_readiness_notes: Vec::new(),
                source_facts: None,
                semantic_facts: Vec::new(),
                warnings: Vec::new(),
                source_based: true,
                semantic_based: false,
            },
            freshness,
        ));
    }

    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut semantic_facts = semantic_facts(ctx, &req).await?;
    let source = source_facts(mathlib_root.path.as_path(), &target)?;
    let ranked = rank_files(&source.matches);
    let likely_file = ranked
        .first()
        .map(|(file, _)| file_to_string(mathlib_root.path.as_path(), file));
    let likely_namespace = likely_file
        .as_ref()
        .and_then(|file| first_namespace(mathlib_root.path.join(file)));
    let suggested_imports = likely_file
        .as_ref()
        .map(|file| vec![module_name_for_mathlib_file(file)])
        .unwrap_or_default();
    let duplicate_terms = duplicate_terms(&req);
    let possible_duplicates = source
        .matches
        .iter()
        .filter(|hit| duplicate_terms.iter().any(|term| contains_word(&hit.text, term)))
        .take(limit)
        .map(|hit| project_hit(mathlib_root.path.as_path(), hit))
        .collect::<Vec<_>>();
    let nearby_declarations = source
        .matches
        .iter()
        .filter(|hit| looks_like_declaration(&hit.text))
        .take(limit)
        .map(|hit| project_hit(mathlib_root.path.as_path(), hit))
        .collect::<Vec<_>>();
    let naming_examples = source
        .matches
        .iter()
        .filter(|hit| looks_like_declaration(&hit.text) && target.iter().any(|term| contains_word(&hit.text, term)))
        .take(limit)
        .map(|hit| project_hit(mathlib_root.path.as_path(), hit))
        .collect::<Vec<_>>();
    if semantic_facts.is_empty() {
        semantic_facts.push("no Lean-semantic sampling was available from the supplied target".to_owned());
    }

    let warnings = if source.truncated {
        vec!["source scan hit a configured cap; placement advice is incomplete".to_owned()]
    } else {
        Vec::new()
    };
    Ok(Response::ok(
        MathlibPlacementResult {
            status: "placement".to_owned(),
            message: None,
            checked: Vec::new(),
            likely_namespace,
            likely_file,
            suggested_imports,
            nearby_declarations,
            possible_duplicates,
            naming_examples,
            upstream_readiness_notes: upstream_notes(),
            source_facts: Some(Box::new(PlacementSourceFacts {
                mathlib_root: mathlib_root.path.to_string_lossy().into_owned(),
                files_scanned: source.files_scanned,
                files_skipped: source.files_skipped,
                matches_considered: source.matches.len(),
                truncated: source.truncated,
                source_based: true,
            })),
            semantic_facts,
            warnings,
            source_based: true,
            semantic_based: true,
        },
        freshness,
    ))
}

fn discover_mathlib_root(req: &MathlibPlacementRequest, project_root: &Path) -> Option<MathlibRoot> {
    let candidates = mathlib_root_candidates(req, project_root);
    candidates
        .into_iter()
        .find(|path| is_mathlib_root(path))
        .map(|path| MathlibRoot { path })
}

fn checked_mathlib_roots(req: &MathlibPlacementRequest, project_root: &Path) -> Vec<String> {
    mathlib_root_candidates(req, project_root)
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn mathlib_root_candidates(req: &MathlibPlacementRequest, project_root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = &req.mathlib_root {
        candidates.push(root.clone());
    }
    candidates.push(project_root.join("Mathlib"));
    candidates.push(project_root.join(".lake/packages/mathlib/Mathlib"));
    candidates
}

fn is_mathlib_root(path: &Path) -> bool {
    path.is_dir() && path.join("Init.lean").exists() || path.is_dir() && path.join("Data").is_dir()
}

async fn semantic_facts(ctx: &ToolContext, req: &MathlibPlacementRequest) -> Result<Vec<String>> {
    let mut facts = Vec::new();
    if req.name.as_deref().is_some_and(|name| !name.trim().is_empty())
        || req.file.is_some() && req.line.is_some() && req.column.is_some()
    {
        let inspected = inspect_declaration(
            ctx,
            InspectDeclarationRequest {
                name: req.name.clone(),
                file: req.file.clone(),
                line: req.line,
                column: req.column,
                imports: Vec::new(),
                project: req.project.clone(),
                fields: InspectDeclarationFields {
                    source: true,
                    statement: true,
                    docstring: false,
                    attributes: true,
                    flags: true,
                },
                max_field_bytes: Some(2048),
                max_total_bytes: Some(4096),
            },
        )
        .await?;
        match inspected.result {
            DeclarationInspectionResult::Found { declaration } => {
                facts.push(format!(
                    "inspected selected declaration: {} ({})",
                    declaration.name, declaration.kind
                ));
                if let Some(module) = declaration.module {
                    facts.push(format!("selected declaration module: {module}"));
                }
                if declaration
                    .statement
                    .as_ref()
                    .is_some_and(|statement| statement.truncated)
                {
                    facts.push("selected declaration statement was truncated for placement sampling".to_owned());
                }
            }
            DeclarationInspectionResult::NotFound { .. } => {
                facts.push("selected declaration was not found by Lean inspection".to_owned());
            }
            DeclarationInspectionResult::Ambiguous { candidates } => {
                facts.push(format!(
                    "cursor resolved ambiguously to {} declarations",
                    candidates.len()
                ));
            }
            DeclarationInspectionResult::Unsupported => {
                facts.push("Lean declaration inspection is unsupported by the loaded worker shim".to_owned());
            }
        }
    }

    if let Some(statement) = req.statement.as_ref().filter(|value| !value.trim().is_empty()) {
        let search = search_for_proof(
            ctx,
            SearchForProofRequest {
                file: None,
                line: None,
                column: None,
                goal: None,
                type_text: Some(statement.clone()),
                imports: Vec::new(),
                mode: None,
                limit: Some(5),
                project: req.project.clone(),
            },
        )
        .await?;
        let names = search
            .result
            .candidates
            .iter()
            .take(5)
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if names.is_empty() {
            facts.push("semantic proof search found no close declaration candidates for the statement".to_owned());
        } else {
            facts.push(format!("semantic proof search candidates: {names}"));
        }
    }

    Ok(facts)
}

fn source_facts(root: &Path, terms: &[String]) -> Result<SourcePlacementFacts> {
    let mut facts = SourcePlacementFacts::default();
    'walk: for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !is_ignored_dir(entry.file_name().to_str().unwrap_or("")))
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() || entry.path().extension().and_then(|ext| ext.to_str()) != Some("lean") {
            continue;
        }
        if facts.files_scanned >= MAX_FILES_SCANNED {
            facts.truncated = true;
            break;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            facts.files_skipped = facts.files_skipped.saturating_add(1);
            continue;
        };
        facts.files_scanned = facts.files_scanned.saturating_add(1);
        for (index, line) in contents.lines().enumerate() {
            let reason = if looks_like_declaration(line) && terms.iter().any(|term| contains_word(line, term)) {
                "nearby_declaration"
            } else if terms.iter().any(|term| contains_word(line, term)) {
                "concept_match"
            } else {
                continue;
            };
            facts.matches.push(SourceMatch {
                file: entry.path().to_path_buf(),
                line: u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX),
                text: line.trim_end().to_owned(),
                reason: reason.to_owned(),
            });
            if facts.matches.len() >= MAX_MATCHES_CONSIDERED {
                facts.truncated = true;
                break 'walk;
            }
        }
    }
    Ok(facts)
}

fn target_terms(req: &MathlibPlacementRequest) -> Vec<String> {
    let mut terms = BTreeSet::new();
    for value in [
        req.name.as_deref(),
        req.proposed_name.as_deref(),
        req.statement.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        for token in extract_tokens(value) {
            terms.insert(token);
        }
    }
    for concept in &req.concepts {
        for token in extract_tokens(concept) {
            terms.insert(token);
        }
    }
    let mut out = terms.into_iter().filter(|token| token.len() >= 3).collect::<Vec<_>>();
    out.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    out.truncate(12);
    out
}

fn duplicate_terms(req: &MathlibPlacementRequest) -> Vec<String> {
    [req.proposed_name.as_deref(), req.name.as_deref()]
        .into_iter()
        .flatten()
        .flat_map(extract_tokens)
        .filter(|token| token.len() >= 3)
        .collect()
}

fn extract_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.is_empty() {
            if ch.is_ascii_alphabetic() {
                current.push(ch);
            }
        } else if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '.') {
            current.push(ch);
        } else {
            tokens.push(current.trim_matches('.').to_owned());
            current.clear();
        }
    }
    if !current.is_empty() {
        tokens.push(current.trim_matches('.').to_owned());
    }
    tokens.into_iter().filter(|token| !is_stop_word(token)).collect()
}

fn is_stop_word(token: &str) -> bool {
    matches!(
        token,
        "theorem" | "lemma" | "def" | "by" | "Prop" | "Type" | "Sort" | "forall" | "fun" | "where" | "let"
    )
}

fn rank_files(matches: &[SourceMatch]) -> Vec<(PathBuf, usize)> {
    let mut counts = BTreeMap::<PathBuf, usize>::new();
    for hit in matches {
        let weight = if looks_like_declaration(&hit.text) { 3 } else { 1 };
        let count = counts.entry(hit.file.clone()).or_default();
        *count = count.saturating_add(weight);
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
}

fn first_namespace(path: PathBuf) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix("namespace ")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn file_to_string(root: &Path, file: &Path) -> String {
    file.strip_prefix(root).unwrap_or(file).to_string_lossy().into_owned()
}

fn module_name_for_mathlib_file(file: &str) -> String {
    let without_ext = file.strip_suffix(".lean").unwrap_or(file);
    format!("Mathlib.{}", without_ext.replace('/', "."))
}

fn project_hit(root: &Path, hit: &SourceMatch) -> PlacementSourceHit {
    PlacementSourceHit {
        file: file_to_string(root, &hit.file),
        line: hit.line,
        text: hit.text.clone(),
        reason: hit.reason.clone(),
    }
}

fn looks_like_declaration(line: &str) -> bool {
    let trimmed = line.trim_start();
    [
        "def ",
        "theorem ",
        "lemma ",
        "class ",
        "structure ",
        "inductive ",
        "instance ",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

fn contains_word(text: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    text.find(term).is_some_and(|start| {
        let end = start.saturating_add(term.len());
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        !before.is_some_and(is_name_char) && !after.is_some_and(is_name_char)
    })
}

fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '\'' | '.')
}

fn upstream_notes() -> Vec<String> {
    vec![
        "Placement is source-based advice; inspect nearby declarations before adding a new theorem.".to_owned(),
        "Prefer the closest existing Mathlib namespace and the narrowest import, not `import Mathlib`.".to_owned(),
        "Search for equivalent forms before adding a local wrapper or duplicate theorem.".to_owned(),
        "Before upstreaming, verify no `sorry`/`admit`, no project-only axioms, and clean Mathlib-style naming."
            .to_owned(),
    ]
}
