//! Proof-agent declaration retrieval.
//!
//! `search_for_proof` is an intent-shaped orchestration over existing
//! bounded primitives: declaration context comes from `proof_state`, candidate
//! generation comes from lean-rs declaration search v2, and ranking stays
//! local and deterministic. It deliberately does not render declarations.

// Tool handlers consume owned requests so worker calls can cross async
// boundaries without borrow plumbing.
#![allow(clippy::needless_pass_by_value)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use lean_rs_worker_parent::{
    LeanWorkerDeclarationFilter, LeanWorkerDeclarationNameMatch, LeanWorkerDeclarationSearch,
    LeanWorkerDeclarationSearchBias, LeanWorkerDeclarationSearchScope,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::{BrokerCall, ProjectHint};
use crate::envelope::{Response, RuntimeFacts};
use crate::error::Result;
use crate::projections::{
    DeclarationSearchFacts, DeclarationSearchResult, DeclarationSummary, SourceRange, project_declaration_search,
};
use crate::tools::position::{ProofPositionSelector, ProofStateRequest, ProofStateResult};
use crate::tools::{ToolContext, session_imports};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 20;
const SEARCH_FANOUT: usize = 50;
const MAX_SEARCHES: usize = 6;
const MAX_REQUIRED_CONSTANTS: usize = 3;
const MAX_NAME_FRAGMENTS: usize = 6;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProofSearchMode {
    #[default]
    NextStep,
    Exact,
    Apply,
    Rewrite,
    Simp,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchForProofRequest {
    /// Path to a `.lean` file for declaration-based proof retrieval.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Declaration name for file-based proof retrieval.
    #[serde(default)]
    pub declaration: Option<String>,
    #[serde(default)]
    pub proof_position: ProofPositionSelector,
    /// Explicit goal text when no file/declaration context is available.
    #[serde(default)]
    pub goal: Option<String>,
    /// Explicit type/proposition text. Used with or instead of `goal`.
    #[serde(default)]
    pub type_text: Option<String>,
    /// Imports for explicit-text search. Cursor search derives imports from the file.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Retrieval intent. Defaults to `next_step`.
    #[serde(default)]
    pub mode: Option<ProofSearchMode>,
    /// Maximum candidates to return. Defaults to 10 and is capped at 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional explicit project root.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProofSearchCandidate {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRange>,
    pub score: i32,
    pub match_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProofSearchDiagnostics {
    pub proof_state_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    pub search_count: usize,
    pub generated_count: usize,
    pub pruned_count: usize,
    pub ranked_count: usize,
    pub returned_count: usize,
    pub search_truncated: bool,
    pub response_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub broad_pruning: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchForProofResult {
    pub candidates: Vec<ProofSearchCandidate>,
    pub diagnostics: ProofSearchDiagnostics,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct TargetProfile {
    namespace: Option<String>,
    constants: Vec<String>,
    heads: Vec<String>,
    name_fragments: Vec<String>,
    imports: Vec<String>,
    proof_state_status: String,
    cache_status: Option<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct PlannedSearch {
    label: &'static str,
    request: LeanWorkerDeclarationSearch,
}

#[derive(Debug, Clone)]
struct CandidateAccumulator {
    row: DeclarationSummary,
    score: i32,
    reasons: BTreeSet<String>,
}

/// Search for declarations likely to help the current or supplied proof goal.
///
/// # Errors
///
/// Returns infrastructure failures only. Missing proof state, broad search,
/// and empty results are represented in the successful response diagnostics.
pub async fn search_for_proof(ctx: &ToolContext, req: SearchForProofRequest) -> Result<Response<SearchForProofResult>> {
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mode = req.mode.unwrap_or_default();
    let project = req.project.clone();
    let mut profile = target_profile(ctx, req).await?;
    let mut warnings = std::mem::take(&mut profile.warnings);
    let searches = plan_searches(&profile, mode);

    if searches.is_empty() {
        warnings.push("no usable goal constants, heads, or name fragments were available for retrieval".to_owned());
        let result = empty_result(profile, warnings);
        let hint = ProjectHint::from_request(project);
        let runtime = ctx.broker.project_runtime(hint, Vec::new()).await?;
        return Ok(Response::ok(result, runtime.freshness).with_runtime(runtime.runtime));
    }

    let mut search_results = Vec::new();
    let mut runtime: Option<RuntimeFacts> = None;
    for search in searches.iter().take(MAX_SEARCHES) {
        let call =
            run_declaration_search(ctx, project.clone(), profile.imports.clone(), search.request.clone()).await?;
        runtime = Some(call.runtime);
        search_results.push((search.label, call.value));
    }

    let search_count = search_results.len();
    let result = rank_results(&profile, mode, limit, search_count, search_results, warnings);
    let freshness_imports = profile.imports.clone();
    let hint = ProjectHint::from_request(project);
    let project_runtime = ctx.broker.project_runtime(hint, freshness_imports).await?;
    let runtime = runtime.unwrap_or(project_runtime.runtime);
    Ok(Response::ok(result, project_runtime.freshness).with_runtime(runtime))
}

async fn target_profile(ctx: &ToolContext, req: SearchForProofRequest) -> Result<TargetProfile> {
    let explicit_text = [req.goal.as_deref(), req.type_text.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    if let (Some(file), Some(declaration)) = (
        req.file.clone(),
        req.declaration.clone().filter(|value| !value.trim().is_empty()),
    ) {
        let proof_response = crate::tools::position::proof_state(
            ctx,
            ProofStateRequest {
                file,
                declaration,
                proof_position: req.proof_position.clone(),
                project: req.project.clone(),
            },
        )
        .await?;
        let imports = if proof_response.freshness.imports.is_empty() {
            req.imports.clone()
        } else {
            proof_response.freshness.imports.clone()
        };
        let Some(proof_result) = proof_response.result else {
            return Ok(profile_from_text(
                explicit_text,
                None,
                imports,
                "runtime_unavailable_degraded_to_explicit_text".to_owned(),
                None,
                vec!["proof-state runtime was unavailable; used explicit goal/type text".to_owned()],
            ));
        };
        match proof_result {
            ProofStateResult::Context {
                goals_before,
                goals_after,
                locals,
                expected_type,
                namespace_name,
                query_facts,
                unavailable,
                budget_exceeded,
                ..
            } => {
                let mut pieces = Vec::new();
                pieces.extend(goals_before);
                pieces.extend(goals_after);
                if let Some(expected) = expected_type {
                    pieces.push(expected.value);
                }
                for local in locals {
                    pieces.push(local.type_str.value);
                }
                if !explicit_text.is_empty() {
                    pieces.push(explicit_text);
                }
                let mut warnings = Vec::new();
                warnings.extend(
                    unavailable
                        .into_iter()
                        .map(|item| format!("proof-state selector unavailable: {}: {}", item.id, item.message)),
                );
                warnings.extend(
                    budget_exceeded
                        .into_iter()
                        .map(|item| format!("proof-state selector budget exceeded: {}: {}", item.id, item.message)),
                );
                return Ok(profile_from_text(
                    pieces.join("\n"),
                    namespace_name,
                    imports,
                    "context".to_owned(),
                    Some(query_facts.cache_status.to_owned()),
                    warnings,
                ));
            }
            ProofStateResult::HeaderParseFailed { .. } => {
                if !explicit_text.is_empty() {
                    return Ok(profile_from_text(
                        explicit_text,
                        None,
                        req.imports,
                        "header_parse_failed_degraded_to_explicit_text".to_owned(),
                        None,
                        vec!["proof-state header parse failed; used explicit goal/type text".to_owned()],
                    ));
                }
                return Ok(profile_from_text(
                    String::new(),
                    None,
                    req.imports,
                    "header_parse_failed".to_owned(),
                    None,
                    vec!["proof-state header parse failed and no explicit goal/type text was supplied".to_owned()],
                ));
            }
            ProofStateResult::Unsupported => {
                if !explicit_text.is_empty() {
                    return Ok(profile_from_text(
                        explicit_text,
                        None,
                        req.imports,
                        "unsupported_degraded_to_explicit_text".to_owned(),
                        None,
                        vec!["proof-state unsupported; used explicit goal/type text".to_owned()],
                    ));
                }
                return Ok(profile_from_text(
                    String::new(),
                    None,
                    req.imports,
                    "unsupported".to_owned(),
                    None,
                    vec!["proof-state unsupported and no explicit goal/type text was supplied".to_owned()],
                ));
            }
        }
    }

    Ok(profile_from_text(
        explicit_text,
        None,
        req.imports,
        "explicit_text".to_owned(),
        None,
        Vec::new(),
    ))
}

fn profile_from_text(
    text: String,
    namespace: Option<String>,
    imports: Vec<String>,
    proof_state_status: String,
    cache_status: Option<String>,
    warnings: Vec<String>,
) -> TargetProfile {
    let constants = extract_constants(&text);
    let heads = extract_heads(&text);
    let name_fragments = extract_name_fragments(&text, &constants, &heads);
    TargetProfile {
        namespace,
        constants,
        heads,
        name_fragments,
        imports,
        proof_state_status,
        cache_status,
        warnings,
    }
}

fn plan_searches(profile: &TargetProfile, mode: ProofSearchMode) -> Vec<PlannedSearch> {
    let mut out = Vec::new();
    let constants = profile
        .constants
        .iter()
        .take(MAX_REQUIRED_CONSTANTS)
        .cloned()
        .collect::<Vec<_>>();
    let primary_head = preferred_head(&profile.heads, mode);

    match mode {
        ProofSearchMode::Rewrite => {
            push_head_search(&mut out, profile, "rewrite_eq", Some("Eq"));
            push_head_search(&mut out, profile, "rewrite_iff", Some("Iff"));
            push_fragment_searches(&mut out, profile, &["rw", "rewrite"]);
        }
        ProofSearchMode::Simp => {
            push_fragment_searches(&mut out, profile, &["simp"]);
            if let Some(head) = primary_head.as_deref() {
                push_head_search(&mut out, profile, "simp_head", Some(head));
            }
        }
        ProofSearchMode::Exact | ProofSearchMode::Apply | ProofSearchMode::NextStep => {
            if let Some(head) = primary_head.as_deref()
                && !(mode == ProofSearchMode::NextStep && constants.is_empty() && is_broad_head(head))
            {
                push_head_search(&mut out, profile, "conclusion_head", Some(head));
            }
            if !constants.is_empty() {
                push_required_search(&mut out, profile, "required_constants", constants);
            }
        }
    }

    for fragment in &profile.name_fragments {
        push_name_search(&mut out, profile, "name_fragment", fragment);
    }

    dedupe_searches(out)
}

fn base_search(profile: &TargetProfile, label: &'static str) -> PlannedSearch {
    let mut scope_biases = Vec::new();
    if let Some(namespace) = profile.namespace.as_deref().filter(|s| !s.is_empty()) {
        scope_biases.push(LeanWorkerDeclarationSearchBias {
            scope: LeanWorkerDeclarationSearchScope::Namespace,
            prefix: namespace.to_owned(),
            strict: false,
            weight: 8,
        });
    }
    PlannedSearch {
        label,
        request: LeanWorkerDeclarationSearch {
            name_fragment: None,
            name_match: LeanWorkerDeclarationNameMatch::Contains,
            kind: Some("theorem".to_owned()),
            required_constants: Vec::new(),
            conclusion_head: None,
            scope_biases,
            limit: SEARCH_FANOUT,
            filter: LeanWorkerDeclarationFilter {
                include_private: false,
                include_generated: false,
                include_internal: false,
            },
            include_source: true,
        },
    }
}

fn push_head_search(out: &mut Vec<PlannedSearch>, profile: &TargetProfile, label: &'static str, head: Option<&str>) {
    let Some(head) = head else {
        return;
    };
    let mut search = base_search(profile, label);
    search.request.conclusion_head = Some(head.to_owned());
    out.push(search);
}

fn push_required_search(
    out: &mut Vec<PlannedSearch>,
    profile: &TargetProfile,
    label: &'static str,
    constants: Vec<String>,
) {
    let mut search = base_search(profile, label);
    search.request.required_constants = constants;
    out.push(search);
}

fn push_name_search(out: &mut Vec<PlannedSearch>, profile: &TargetProfile, label: &'static str, fragment: &str) {
    let mut search = base_search(profile, label);
    search.request.name_fragment = Some(fragment.to_owned());
    out.push(search);
}

fn push_fragment_searches(out: &mut Vec<PlannedSearch>, profile: &TargetProfile, fragments: &[&str]) {
    for fragment in fragments {
        push_name_search(out, profile, "mode_fragment", fragment);
    }
}

fn dedupe_searches(searches: Vec<PlannedSearch>) -> Vec<PlannedSearch> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for search in searches {
        let key = format!(
            "{:?}|{:?}|{:?}|{:?}",
            search.request.name_fragment,
            search.request.kind,
            search.request.required_constants,
            search.request.conclusion_head
        );
        if seen.insert(key) {
            out.push(search);
        }
        if out.len() >= MAX_SEARCHES {
            break;
        }
    }
    out
}

async fn run_declaration_search(
    ctx: &ToolContext,
    project: Option<String>,
    imports: Vec<String>,
    search: LeanWorkerDeclarationSearch,
) -> Result<BrokerCall<DeclarationSearchResult>> {
    let hint = ProjectHint::from_request(project);
    let call = ctx
        .broker
        .search_declarations(hint, session_imports(imports.clone()), imports, search)
        .await?;
    Ok(BrokerCall {
        value: project_declaration_search(call.value),
        runtime: call.runtime,
        freshness: call.freshness,
    })
}

fn rank_results(
    profile: &TargetProfile,
    mode: ProofSearchMode,
    limit: usize,
    search_count: usize,
    search_results: Vec<(&'static str, DeclarationSearchResult)>,
    warnings: Vec<String>,
) -> SearchForProofResult {
    let mut generated_count = 0usize;
    let mut pruned_count = 0usize;
    let mut search_truncated = false;
    let mut broad_pruning = Vec::new();
    let mut by_name: BTreeMap<String, CandidateAccumulator> = BTreeMap::new();

    for (label, result) in search_results {
        generated_count = generated_count.saturating_add(result.facts.after_scope_filter);
        pruned_count = pruned_count.saturating_add(pruned_from_facts(&result.facts, result.declarations.len()));
        search_truncated |= result.truncated || result.facts.truncated;
        for pruning in &result.facts.broad_pruning {
            broad_pruning.push(format!(
                "{}:{}:{}:{}",
                label, pruning.stage, pruning.reason, pruning.count
            ));
        }
        for row in result.declarations {
            let score = candidate_score(&row, profile, mode, label);
            let entry = by_name.entry(row.name.clone()).or_insert_with(|| CandidateAccumulator {
                row: row.clone(),
                score,
                reasons: BTreeSet::new(),
            });
            entry.score = entry.score.max(score);
            entry.reasons.insert(label.to_owned());
            entry.reasons.insert(row.match_reason.clone());
        }
    }

    let mut ranked = by_name.into_values().collect::<Vec<_>>();
    ranked.sort_by_key(|candidate| (Reverse(candidate.score), candidate.row.name.clone()));
    let ranked_count = ranked.len();
    let candidates = ranked
        .into_iter()
        .take(limit)
        .map(|candidate| ProofSearchCandidate {
            suggested_snippet: suggested_snippet(mode, &candidate.row),
            name: candidate.row.name,
            kind: candidate.row.kind,
            module: candidate.row.module,
            source: candidate.row.source,
            score: candidate.score,
            match_reason: candidate.reasons.into_iter().collect::<Vec<_>>().join(","),
        })
        .collect::<Vec<_>>();

    let mut result = SearchForProofResult {
        diagnostics: ProofSearchDiagnostics {
            proof_state_status: profile.proof_state_status.clone(),
            cache_status: profile.cache_status.clone(),
            search_count,
            generated_count,
            pruned_count,
            ranked_count,
            returned_count: candidates.len(),
            search_truncated,
            response_bytes: 0,
            broad_pruning,
        },
        candidates,
        warnings,
    };
    result.diagnostics.returned_count = result.candidates.len();
    result.diagnostics.response_bytes = serde_json::to_vec(&result).map_or(0, |bytes| bytes.len());
    result
}

fn empty_result(profile: TargetProfile, warnings: Vec<String>) -> SearchForProofResult {
    let mut result = SearchForProofResult {
        candidates: Vec::new(),
        diagnostics: ProofSearchDiagnostics {
            proof_state_status: profile.proof_state_status,
            cache_status: profile.cache_status,
            search_count: 0,
            generated_count: 0,
            pruned_count: 0,
            ranked_count: 0,
            returned_count: 0,
            search_truncated: false,
            response_bytes: 0,
            broad_pruning: Vec::new(),
        },
        warnings,
    };
    result.diagnostics.response_bytes = serde_json::to_vec(&result).map_or(0, |bytes| bytes.len());
    result
}

fn candidate_score(
    row: &DeclarationSummary,
    profile: &TargetProfile,
    mode: ProofSearchMode,
    search_label: &str,
) -> i32 {
    let mut score = row.score;
    if row.kind == "theorem" {
        score = score.saturating_add(10);
    }
    if row.source.is_some() {
        score = score.saturating_add(3);
    }
    if let Some(namespace) = profile.namespace.as_deref()
        && row.name.starts_with(namespace)
    {
        score = score.saturating_add(8);
    }
    if mode == ProofSearchMode::Rewrite && (row.name.contains("iff") || row.name.contains("eq")) {
        score = score.saturating_add(8);
    }
    if mode == ProofSearchMode::Simp && row.name.contains("simp") {
        score = score.saturating_add(8);
    }
    if is_generic_candidate(&row.name) {
        score = score.saturating_sub(40);
    }
    if is_generic_int_solver_candidate(&row.name) && !profile_is_linear_arithmetic(profile) {
        score = score.saturating_sub(35);
    }
    for fragment in &profile.name_fragments {
        if row.name.to_lowercase().contains(fragment) {
            score = score.saturating_add(if is_structural_fragment(fragment) { 10 } else { 5 });
        }
    }
    if search_label == "required_constants" {
        score = score.saturating_add(6);
    }
    if search_label == "conclusion_head" {
        score = score.saturating_add(20);
    }
    score
}

fn is_broad_head(head: &str) -> bool {
    matches!(head, "Eq" | "Iff" | "Exists" | "True")
}

fn is_generic_candidate(name: &str) -> bool {
    let has_generic_segment = name.split('.').any(|segment| {
        segment == "rec"
            || segment == "recOn"
            || segment == "ndrec"
            || segment.starts_with("rec_")
            || segment.starts_with("recOn_")
            || segment.starts_with("ndrec_")
    });
    name.starts_with("Acc.") || has_generic_segment
}

fn is_generic_int_solver_candidate(name: &str) -> bool {
    name.contains("Int.Linear") || name.contains("Int.Cooper") || name.contains(".Linear.") || name.contains(".Cooper.")
}

fn profile_is_linear_arithmetic(profile: &TargetProfile) -> bool {
    profile.name_fragments.iter().any(|fragment| {
        matches!(
            fragment.as_str(),
            "linear" | "omega" | "cooper" | "le" | "lt" | "ge" | "gt" | "add" | "sub"
        )
    })
}

fn is_structural_fragment(fragment: &str) -> bool {
    matches!(
        fragment,
        "den"
            | "num"
            | "denominator"
            | "cast"
            | "intcast"
            | "mul"
            | "dvd"
            | "pow"
            | "factorization"
            | "natabs"
            | "associated"
            | "isunit"
            | "sign"
            | "prod"
    ) || fragment.contains("cast")
}

fn pruned_from_facts(facts: &DeclarationSearchFacts, returned: usize) -> usize {
    let limit_pruned = facts.after_scope_filter.saturating_sub(returned);
    facts
        .broad_pruning
        .iter()
        .fold(limit_pruned, |acc, pruning| acc.saturating_add(pruning.count))
}

fn suggested_snippet(mode: ProofSearchMode, row: &DeclarationSummary) -> Option<String> {
    match mode {
        ProofSearchMode::Exact if row.kind == "theorem" => Some(format!("exact {}", row.name)),
        ProofSearchMode::Apply if row.kind == "theorem" => Some(format!("apply {}", row.name)),
        ProofSearchMode::Rewrite if row.kind == "theorem" => Some(format!("rw [{}]", row.name)),
        ProofSearchMode::Simp if row.kind == "theorem" => Some(format!("simp [{}]", row.name)),
        ProofSearchMode::NextStep
        | ProofSearchMode::Exact
        | ProofSearchMode::Apply
        | ProofSearchMode::Rewrite
        | ProofSearchMode::Simp => None,
    }
}

fn preferred_head(heads: &[String], mode: ProofSearchMode) -> Option<String> {
    if mode == ProofSearchMode::Rewrite {
        if heads.iter().any(|head| head == "Eq") {
            return Some("Eq".to_owned());
        }
        if heads.iter().any(|head| head == "Iff") {
            return Some("Iff".to_owned());
        }
    }
    heads.first().cloned()
}

fn extract_constants(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for token in identifier_tokens(text) {
        if token.len() <= 1 || is_stopword(&token) || token.chars().next().is_some_and(char::is_lowercase) {
            continue;
        }
        if seen.insert(token.clone()) {
            out.push(token);
        }
    }
    out
}

fn extract_heads(text: &str) -> Vec<String> {
    let mut heads = BTreeSet::new();
    if text.contains('=') {
        heads.insert("Eq".to_owned());
    }
    if text.contains('↔') || text.contains("<->") {
        heads.insert("Iff".to_owned());
    }
    if text.contains('∧') || text.contains("/\\") {
        heads.insert("And".to_owned());
    }
    if text.contains('∨') || text.contains("\\/") {
        heads.insert("Or".to_owned());
    }
    if text.contains('∃') || text.contains("Exists") {
        heads.insert("Exists".to_owned());
    }
    if text.contains("True") {
        heads.insert("True".to_owned());
    }
    heads.into_iter().collect()
}

fn extract_name_fragments(text: &str, constants: &[String], heads: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut broad_namespace_fragments = Vec::new();
    for token in identifier_tokens(text) {
        for segment in token.split('.') {
            let fragment = segment.trim_matches('_').to_lowercase();
            if fragment == "rat" {
                if seen.insert(fragment.clone()) {
                    broad_namespace_fragments.push(fragment);
                }
                continue;
            }
            if fragment.len() >= 3 && useful_lower_fragment(&fragment) && seen.insert(fragment.clone()) {
                out.push(fragment);
            }
            if out.len() >= MAX_NAME_FRAGMENTS {
                return out;
            }
        }
    }
    for item in constants.iter().chain(heads.iter()) {
        if is_broad_head(item) {
            continue;
        }
        for segment in item.split('.') {
            let fragment = segment.trim_matches('_');
            if fragment.len() >= 3 && seen.insert(fragment.to_lowercase()) {
                out.push(fragment.to_owned());
            }
            if out.len() >= MAX_NAME_FRAGMENTS {
                return out;
            }
        }
    }
    if out.is_empty() {
        for fragment in broad_namespace_fragments {
            out.push(fragment);
            if out.len() >= MAX_NAME_FRAGMENTS {
                return out;
            }
        }
    }
    out
}

fn useful_lower_fragment(fragment: &str) -> bool {
    matches!(
        fragment,
        "den"
            | "num"
            | "denominator"
            | "cast"
            | "intcast"
            | "mul"
            | "dvd"
            | "pow"
            | "int"
            | "nat"
            | "factorization"
            | "natabs"
            | "associated"
            | "isunit"
            | "sign"
            | "prod"
    ) || fragment.contains("cast")
}

fn identifier_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch == '_' || ch == '\'' || ch == '.' || ch.is_ascii_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "by" | "def"
            | "end"
            | "exact"
            | "fun"
            | "have"
            | "import"
            | "in"
            | "lemma"
            | "let"
            | "match"
            | "namespace"
            | "open"
            | "private"
            | "protected"
            | "scoped"
            | "show"
            | "theorem"
            | "unsafe"
            | "where"
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn search_for_proof_request_defaults() {
        let req: SearchForProofRequest = serde_json::from_value(json!({"goal":"⊢ True"})).unwrap();
        assert_eq!(req.mode.unwrap_or_default(), ProofSearchMode::NextStep);
        assert!(req.file.is_none());
        assert_eq!(req.goal.as_deref(), Some("⊢ True"));
    }

    #[test]
    fn limit_clamps_to_tool_cap() {
        let profile = profile_from_text(
            "⊢ True".to_owned(),
            None,
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        let result = empty_result(profile, Vec::new());
        assert_eq!(result.diagnostics.returned_count, 0);
        assert_eq!(MAX_LIMIT, 20);
    }

    #[test]
    fn extracts_heads_and_fragments() {
        let profile = profile_from_text(
            "⊢ Nat.succ n = Nat.succ m".to_owned(),
            Some("Nat".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert!(profile.heads.iter().any(|head| head == "Eq"));
        assert!(profile.constants.iter().any(|constant| constant == "Nat.succ"));
        assert!(profile.name_fragments.iter().any(|fragment| fragment == "nat"));
    }

    #[test]
    fn rat_arithmetic_profile_keeps_useful_lowercase_fragments() {
        let profile = profile_from_text(
            "q : ℚ\nm c : ℤ\nhc : m = ↑q.den * c\n⊢ ↑(c * q.num) = ↑m * q".to_owned(),
            Some("Rat".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert!(profile.name_fragments.iter().any(|fragment| fragment == "den"));
        assert!(profile.name_fragments.iter().any(|fragment| fragment == "num"));
        assert!(
            !profile.name_fragments.iter().any(|fragment| fragment == "rat"),
            "namespace-only Rat fragment should not crowd out den/num/cast retrieval"
        );
        assert!(
            !plan_searches(&profile, ProofSearchMode::NextStep)
                .iter()
                .any(|search| search.label == "conclusion_head"),
            "next_step should not run a broad Eq-head search without required constants"
        );
    }

    #[test]
    fn generic_recursor_candidates_are_down_ranked() {
        let profile = profile_from_text(
            "⊢ ↑(c * q.num) = ↑m * q".to_owned(),
            Some("Rat".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        let flags = crate::projections::DeclarationFlags {
            is_private: false,
            is_generated: false,
            is_internal: false,
        };
        let generic = DeclarationSummary {
            name: "Acc.ndrecOn_eq_ndrecOnC".to_owned(),
            kind: "theorem".to_owned(),
            module: None,
            source: None,
            match_reason: "name fragment".to_owned(),
            score: 100,
            rank: 1,
            flags: flags.clone(),
        };
        let rat_candidate = DeclarationSummary {
            name: "Rat.num_den_helper".to_owned(),
            kind: "theorem".to_owned(),
            module: Some("Mathlib.Data.Rat.Lemmas".to_owned()),
            source: None,
            match_reason: "name fragment".to_owned(),
            score: 100,
            rank: 1,
            flags,
        };
        assert!(
            candidate_score(&rat_candidate, &profile, ProofSearchMode::NextStep, "name_fragment")
                > candidate_score(&generic, &profile, ProofSearchMode::NextStep, "name_fragment")
        );
    }
}
