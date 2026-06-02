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
const MAX_NAME_FRAGMENTS: usize = 12;

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
    /// Where in the proof to read the goal; defaults to the pristine entry goal
    /// (before any tactic runs). See [`ProofPositionSelector`].
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

/// How the candidate set interpretation context an agent acts on, plus the
/// optional search funnel an operator reads when tuning retrieval.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProofSearchDiagnostics {
    /// Where the goal text came from: `context` (live proof state) or a degraded
    /// fallback (e.g. `explicit_text`). Tells the agent how much to trust ranking.
    pub proof_state_status: String,
    pub returned_count: usize,
    /// A search hit its fan-out cap, so the candidate set may be incomplete.
    pub search_truncated: bool,
    /// Retrieval funnel counts and cache status. Pure operational telemetry;
    /// emitted only under `telemetry.verbosity = full`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub funnel: Option<SearchFunnel>,
}

/// The retrieval funnel: how many declarations each stage produced and pruned.
/// Operator-facing tuning signal, not something a proof step depends on.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchFunnel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    pub search_count: usize,
    pub generated_count: usize,
    pub pruned_count: usize,
    pub ranked_count: usize,
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
    kind: GoalProfileKind,
    namespace: Option<String>,
    constants: Vec<String>,
    heads: Vec<String>,
    name_fragments: Vec<String>,
    imports: Vec<String>,
    proof_state_status: String,
    cache_status: Option<String>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum GoalProfileKind {
    RatArithmetic,
    IntFactorization,
    ModelTheoryRelabel,
    LinearArithmetic,
    Generic,
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
    let full = ctx.config.verbosity.is_full();
    let project = req.project.clone();
    let mut profile = target_profile(ctx, req).await?;
    let mut warnings = std::mem::take(&mut profile.warnings);
    let searches = plan_searches(&profile, mode);

    if searches.is_empty() {
        warnings.push("no usable goal constants, heads, or name fragments were available for retrieval".to_owned());
        let result = empty_result(profile, warnings, full);
        let hint = ProjectHint::from_request(project);
        let runtime = ctx.broker.project_runtime(hint, Vec::new()).await?;
        return Ok(Response::ok(result, runtime.freshness).with_runtime(runtime.runtime));
    }

    let mut search_results = Vec::new();
    let mut runtime: Option<RuntimeFacts> = None;
    for search in searches.iter().take(MAX_SEARCHES) {
        let call = match run_declaration_search(ctx, project.clone(), profile.imports.clone(), search.request.clone())
            .await
        {
            Ok(call) => call,
            Err(err) if crate::diagnosis::missing_olean_failure(&err) => {
                let result = empty_result(profile.clone(), warnings, full);
                let hint = ProjectHint::from_request(project);
                let project_runtime = ctx.broker.project_runtime(hint, profile.imports.clone()).await?;
                let response = Response::ok(result, project_runtime.freshness).with_runtime(project_runtime.runtime);
                let response = crate::diagnosis::warn_needs_build(
                    response,
                    &crate::diagnosis::IncompleteCause::MissingOlean(err.to_string()),
                );
                return Ok(response
                    .hint("supply a valid file or explicit imports; search_for_proof will not broad-import Mathlib as fallback"));
            }
            Err(err) => return Err(err),
        };
        runtime = Some(call.runtime);
        search_results.push((search.label, call.value));
    }

    let search_count = search_results.len();
    let result = rank_results(&profile, mode, limit, search_count, search_results, warnings, full);
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
        let imports = if proof_response.imports().is_empty() {
            req.imports.clone()
        } else {
            proof_response.imports().to_vec()
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
                    query_facts.map(|facts| facts.cache_status.to_owned()),
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
    let kind = classify_goal_profile(&text, &constants);
    let name_fragments = extract_name_fragments(&text, &constants, &heads, kind);
    TargetProfile {
        kind,
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
                // A specific relational head (`LE.le`, `Membership.mem`, …) is a
                // meaningful filter and labels the search `conclusion_head`, which
                // counts as corroboration. A broad head (`Eq`/`Iff`/…) matches
                // nearly everything, so it gets a distinct label and does *not*
                // corroborate — otherwise generic `*_eq_*` lemmas would outrank
                // domain name-fragment matches.
                let label = if is_broad_head(head) {
                    "broad_conclusion_head"
                } else {
                    "conclusion_head"
                };
                push_head_search(&mut out, profile, label, Some(head));
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
    full: bool,
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

    // Name-fragment bonus, applied once per candidate now that the full set of
    // corroborating searches is known. The structural `+10` is earned only when
    // a head or required-constants search also vouches for the candidate; a bare
    // lexical hit gets the smaller `+5` and no structural lift.
    for entry in by_name.values_mut() {
        let corroborated = is_corroborated(&entry.reasons);
        let lower = entry.row.name.to_lowercase();
        let mut bonus = 0i32;
        for fragment in &profile.name_fragments {
            if lower.contains(fragment) {
                bonus = bonus.saturating_add(if corroborated && is_structural_fragment(fragment) {
                    10
                } else {
                    5
                });
            }
        }
        entry.score = entry.score.saturating_add(bonus);
    }

    // When any candidate is head-/constant-corroborated, sink the purely lexical
    // ones below all of them: the worker's +40 name-suffix base outscores its +30
    // conclusion-head base, so a flat penalty cannot reliably demote noise.
    let any_corroborated = by_name.values().any(|entry| is_corroborated(&entry.reasons));
    let mut ranked = by_name.into_values().collect::<Vec<_>>();
    ranked.sort_by_key(|candidate| {
        let demoted = any_corroborated && !is_corroborated(&candidate.reasons);
        (demoted, Reverse(candidate.score), candidate.row.name.clone())
    });
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

    // Honesty signal: if the goal carried domain name-fragments but no returned
    // candidate's name matches any of them, the results are conclusion-head
    // matches (e.g. generic `Iff`/`eq` lemmas), not domain-relevant. Say so
    // rather than letting the agent mistake noise for guidance.
    let mut warnings = warnings;
    if !candidates.is_empty()
        && !profile.name_fragments.is_empty()
        && !candidates.iter().any(|candidate| {
            let lower = candidate.name.to_lowercase();
            profile.name_fragments.iter().any(|fragment| lower.contains(fragment))
        })
    {
        warnings.push(
            "results match the goal's conclusion head only, not its domain terms; for a known target prefer \
             find_references or loogle over search_for_proof"
                .to_owned(),
        );
    }

    // Honesty signal, complementary to the above: if every returned candidate's
    // only evidence is a name-fragment substring — no conclusion-head and no
    // required-constant search vouched for it — the results are lexical guesses.
    // Mutually exclusive with the head-only warning by construction.
    if !candidates.is_empty()
        && candidates.iter().all(|candidate| {
            !candidate.match_reason.contains("conclusion_head")
                && !candidate.match_reason.contains("required_constants")
        })
    {
        warnings.push(
            "results matched the goal by name fragment only, with no conclusion-head or required-constant \
             corroboration; they are lexical guesses and may not match the goal — for a known target prefer \
             find_references or loogle over search_for_proof"
                .to_owned(),
        );
    }

    let funnel = full.then(|| SearchFunnel {
        cache_status: profile.cache_status.clone(),
        search_count,
        generated_count,
        pruned_count,
        ranked_count,
        response_bytes: 0,
        broad_pruning,
    });
    let mut result = SearchForProofResult {
        diagnostics: ProofSearchDiagnostics {
            proof_state_status: profile.proof_state_status.clone(),
            returned_count: candidates.len(),
            search_truncated,
            funnel,
        },
        candidates,
        warnings,
    };
    result.diagnostics.returned_count = result.candidates.len();
    record_response_bytes(&mut result);
    result
}

fn empty_result(profile: TargetProfile, warnings: Vec<String>, full: bool) -> SearchForProofResult {
    let funnel = full.then(|| SearchFunnel {
        cache_status: profile.cache_status,
        search_count: 0,
        generated_count: 0,
        pruned_count: 0,
        ranked_count: 0,
        response_bytes: 0,
        broad_pruning: Vec::new(),
    });
    let mut result = SearchForProofResult {
        candidates: Vec::new(),
        diagnostics: ProofSearchDiagnostics {
            proof_state_status: profile.proof_state_status,
            returned_count: 0,
            search_truncated: false,
            funnel,
        },
        warnings,
    };
    record_response_bytes(&mut result);
    result
}

/// Stamp the funnel's `response_bytes` with the serialized result size, when the
/// funnel is present. A self-referential measurement, so it runs last.
fn record_response_bytes(result: &mut SearchForProofResult) {
    if result.diagnostics.funnel.is_some() {
        let bytes = serde_json::to_vec(result).map_or(0, |bytes| bytes.len());
        if let Some(funnel) = result.diagnostics.funnel.as_mut() {
            funnel.response_bytes = bytes;
        }
    }
}

/// Whether a candidate's accumulated evidence includes a *specific* head or a
/// required-constants search — i.e. something type-aware vouched for it, not just
/// a name-substring or a broad `Eq`/`Iff` head (which match nearly everything).
///
/// Keyed on the exact parent search label: the worker stamps `conclusion_head`
/// into its `match_reason` for broad heads too, so a substring test would let
/// generic `*_eq_*` lemmas masquerade as corroborated. Broad-head searches carry
/// the distinct `broad_conclusion_head` label and deliberately do not match here.
fn is_corroborated(reasons: &BTreeSet<String>) -> bool {
    reasons
        .iter()
        .any(|reason| reason == "conclusion_head" || reason == "required_constants")
}

fn candidate_score(
    row: &DeclarationSummary,
    profile: &TargetProfile,
    mode: ProofSearchMode,
    search_label: &str,
) -> i32 {
    let mut score = row.score;
    let lower_name = row.name.to_lowercase();
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
    if let (Some(namespace), Some(module)) = (profile.namespace.as_deref(), row.module.as_deref())
        && namespace
            .split('.')
            .next()
            .is_some_and(|root| !root.is_empty() && module.contains(root))
    {
        score = score.saturating_add(4);
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
    if is_generic_additive_or_cast_candidate(&lower_name) && !profile_allows_generic_additive_or_cast(profile) {
        score = score.saturating_sub(20);
    }
    if is_structural_noise_candidate(&lower_name, row.module.as_deref(), profile) {
        score = score.saturating_sub(45);
    }
    score = score.saturating_add(profile_specific_score_adjustment(&lower_name, profile));
    if search_label == "required_constants" {
        score = score.saturating_add(6);
    }
    if search_label == "conclusion_head" {
        score = score.saturating_add(20);
    }
    score
}

fn profile_specific_score_adjustment(lower_name: &str, profile: &TargetProfile) -> i32 {
    match profile.kind {
        GoalProfileKind::RatArithmetic => {
            topical_adjustment(lower_name, &["rat", "den", "num", "denominator", "intcast"], 22, -8)
        }
        GoalProfileKind::IntFactorization => topical_adjustment(
            lower_name,
            &[
                "factorization",
                "factor",
                "prime",
                "irreducible",
                "multiplicity",
                "normalizedfactors",
                "associated",
                "isunit",
                "dvd",
            ],
            24,
            -12,
        ),
        GoalProfileKind::ModelTheoryRelabel => topical_adjustment(
            lower_name,
            &[
                "relabel",
                "bounded",
                "formula",
                "language",
                "firstorder",
                "realize",
                "theory",
            ],
            24,
            -12,
        ),
        GoalProfileKind::LinearArithmetic => topical_adjustment(
            lower_name,
            &[
                "int.linear",
                "int.cooper",
                "cooper",
                "omega",
                "linarith",
                "linear",
                "le_of",
                "lt_of",
                "add_le",
                "le_add",
                "sub_le",
                "le_sub",
                "nonneg",
                "nonpos",
            ],
            28,
            -6,
        ),
        GoalProfileKind::Generic => 0,
    }
}

fn topical_adjustment(lower_name: &str, topical_fragments: &[&str], boost: i32, miss_penalty: i32) -> i32 {
    if topical_fragments.iter().any(|fragment| lower_name.contains(fragment)) {
        boost
    } else {
        miss_penalty
    }
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

fn is_generic_additive_or_cast_candidate(lower_name: &str) -> bool {
    contains_any(
        lower_name,
        &[
            "addmonoidhom",
            "addmonoid",
            "addcomm",
            "int.cast",
            "nat.cast",
            "zsmul",
            "nsmul",
            "coe",
            "cast",
        ],
    )
}

fn is_structural_noise_candidate(lower_name: &str, module: Option<&str>, profile: &TargetProfile) -> bool {
    let lower_module = module.unwrap_or_default().to_lowercase();
    let mentions_data = profile_mentions_any(profile, &["array", "list", "vector", "getelem"]);
    if !mentions_data
        && (contains_any(
            lower_name,
            &["array.", "list.", "vector.", "getelem", "getelem?", "uget"],
        ) || contains_any(&lower_module, &["init.data.array", "data.array", "data.list"]))
    {
        return true;
    }

    let mentions_order_morphism = profile_mentions_any(profile, &["antitone", "monotone", "strictmono", "strictanti"]);
    if !mentions_order_morphism
        && contains_any(
            lower_name,
            &["antitone.", "monotone.", "strictmono.", "strictanti.", "reflect_lt"],
        )
    {
        return true;
    }
    false
}

fn profile_mentions_any(profile: &TargetProfile, needles: &[&str]) -> bool {
    profile
        .name_fragments
        .iter()
        .chain(profile.constants.iter())
        .any(|item| {
            let lower = item.to_lowercase();
            needles.iter().any(|needle| lower.contains(needle))
        })
}

fn profile_allows_generic_additive_or_cast(profile: &TargetProfile) -> bool {
    matches!(
        profile.kind,
        GoalProfileKind::RatArithmetic | GoalProfileKind::LinearArithmetic
    ) || profile.name_fragments.iter().any(|fragment| {
        matches!(
            fragment.as_str(),
            "cast" | "intcast" | "coe" | "add" | "sub" | "linear" | "omega" | "cooper"
        )
    })
}

fn profile_is_linear_arithmetic(profile: &TargetProfile) -> bool {
    if profile.kind == GoalProfileKind::LinearArithmetic {
        return true;
    }
    profile.name_fragments.iter().any(|fragment| {
        matches!(
            fragment.as_str(),
            "linear" | "omega" | "cooper" | "le" | "lt" | "ge" | "gt" | "add" | "sub"
        )
    })
}

fn classify_goal_profile(text: &str, constants: &[String]) -> GoalProfileKind {
    let lower = text.to_lowercase();
    let constant_text = constants
        .iter()
        .map(|constant| constant.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let haystack = format!("{lower} {constant_text}");
    if contains_any(
        &haystack,
        &["rat", "ℚ", "denominator", ".den", ".num", "num", "intcast"],
    ) {
        return GoalProfileKind::RatArithmetic;
    }
    if contains_any(
        &haystack,
        &[
            "factorization",
            "factorisation",
            "prime",
            "irreducible",
            "multiplicity",
            "normalizedfactors",
            "associated",
            "isunit",
            "dvd",
            "nat.factor",
            "int.factor",
        ],
    ) {
        return GoalProfileKind::IntFactorization;
    }
    if contains_any(
        &haystack,
        &[
            "relabel",
            "bounded",
            "formula",
            "firstorder",
            "language",
            "structure",
            "term",
            "realize",
            "lhom",
            "theory",
        ],
    ) {
        return GoalProfileKind::ModelTheoryRelabel;
    }
    if contains_any(
        &haystack,
        &[
            "int.linear",
            "cooper",
            "omega",
            "linarith",
            "linear",
            "≤",
            "<=",
            "≥",
            ">=",
            " < ",
            " > ",
        ],
    ) {
        return GoalProfileKind::LinearArithmetic;
    }
    GoalProfileKind::Generic
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
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
            | "factor"
            | "prime"
            | "irreducible"
            | "multiplicity"
            | "natabs"
            | "associated"
            | "isunit"
            | "sign"
            | "prod"
            | "relabel"
            | "bounded"
            | "formula"
            | "language"
    )
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
    // Prefer a specific relational head (`LE.le`, `Membership.mem`, …) over a
    // broad one (`Eq`/`Iff`/`Exists`/`True`): a hypothesis `=` must not let `Eq`
    // (alphabetically first in the `BTreeSet`) win and then get dropped by the
    // broad-head guard in `plan_searches`.
    heads
        .iter()
        .find(|head| !is_broad_head(head))
        .or_else(|| heads.first())
        .cloned()
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
    if text.contains('≠') {
        heads.insert("Ne".to_owned());
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
    // Relational/membership/order notation maps to the head constant the worker
    // filters on (`conclusionHead?` = `getAppFn` of the fully-quantified
    // conclusion, matched by exact `Name` equality). Without these, an `LE.le`
    // goal yields no head and the one type-aware worker filter never runs.
    if text.contains('≤') || text.contains("<=") {
        heads.insert("LE.le".to_owned());
    }
    if text.contains('≥') || text.contains(">=") {
        heads.insert("GE.ge".to_owned());
    }
    if text.contains('∈') {
        heads.insert("Membership.mem".to_owned());
    }
    if text.contains('∣') {
        heads.insert("Dvd.dvd".to_owned());
    }
    if text.contains('⊆') {
        heads.insert("HasSubset.Subset".to_owned());
    }
    // Bare ASCII `<`/`>` are the pretty-printer's rendering of `LT.lt`/`GT.gt`,
    // but they also appear inside `<=`, `>=`, `<->`, and the `->`/`<-` arrows;
    // only count a `<`/`>` that is not part of one of those sequences.
    if has_bare_relation(text, '<') {
        heads.insert("LT.lt".to_owned());
    }
    if has_bare_relation(text, '>') {
        heads.insert("GT.gt".to_owned());
    }
    heads.into_iter().collect()
}

/// Whether `text` contains a `<`/`>` that denotes `LT.lt`/`GT.gt` rather than a
/// fragment of `<=`, `>=`, `<->`, or an ASCII arrow (`->`, `<-`).
fn has_bare_relation(text: &str, target: char) -> bool {
    let chars = text.chars().collect::<Vec<_>>();
    chars.iter().enumerate().any(|(i, &c)| {
        if c != target {
            return false;
        }
        let next = i.checked_add(1).and_then(|j| chars.get(j)).copied();
        let prev = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
        match target {
            // `<=` (LE.le) and `<-`/`<->` (arrows/Iff) are not LT.lt.
            '<' => next != Some('=') && next != Some('-'),
            // `>=` (GE.ge) and a trailing `>` of `->`/`<->` are not GT.gt.
            '>' => next != Some('=') && prev != Some('-'),
            _ => false,
        }
    })
}

/// Heads `extract_heads` synthesizes from notation. These are structural
/// operators, not domain terms, so they must not seed name-fragment searches.
fn is_notation_head(head: &str) -> bool {
    matches!(
        head,
        "Eq" | "Ne"
            | "Iff"
            | "And"
            | "Or"
            | "Exists"
            | "True"
            | "LE.le"
            | "GE.ge"
            | "LT.lt"
            | "GT.gt"
            | "Membership.mem"
            | "Dvd.dvd"
            | "HasSubset.Subset"
    )
}

fn extract_name_fragments(text: &str, constants: &[String], heads: &[String], kind: GoalProfileKind) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut broad_namespace_fragments = Vec::new();
    for fragment in curated_fragments(kind) {
        if seen.insert((*fragment).to_owned()) {
            out.push((*fragment).to_owned());
        }
    }
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
        if is_notation_head(item) {
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

fn curated_fragments(kind: GoalProfileKind) -> &'static [&'static str] {
    match kind {
        GoalProfileKind::RatArithmetic => &["den", "num", "denominator", "intcast", "cast"],
        GoalProfileKind::IntFactorization => &[
            "factorization",
            "factor",
            "prime",
            "irreducible",
            "multiplicity",
            "normalizedfactors",
            "associated",
            "isunit",
            "dvd",
        ],
        GoalProfileKind::ModelTheoryRelabel => &[
            "relabel",
            "bounded",
            "formula",
            "language",
            "firstorder",
            "realize",
            "term",
            "theory",
        ],
        GoalProfileKind::LinearArithmetic => &[
            "int.linear",
            "cooper",
            "omega",
            "linarith",
            "linear",
            "add_le",
            "le_add",
            "sub_le",
            "le_sub",
            "le",
            "lt",
        ],
        GoalProfileKind::Generic => &[],
    }
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
            | "factor"
            | "prime"
            | "irreducible"
            | "multiplicity"
            | "normalizedfactors"
            | "natabs"
            | "associated"
            | "isunit"
            | "sign"
            | "prod"
            | "relabel"
            | "bounded"
            | "formula"
            | "firstorder"
            | "language"
            | "realize"
            | "theory"
            | "term"
            | "linear"
            | "cooper"
            | "omega"
            | "linarith"
            | "add_le"
            | "le_add"
            | "sub_le"
            | "le_sub"
    )
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

    fn summary(name: &str, module: Option<&str>, score: i32) -> DeclarationSummary {
        DeclarationSummary {
            name: name.to_owned(),
            kind: "theorem".to_owned(),
            module: module.map(ToOwned::to_owned),
            source: None,
            match_reason: "name fragment".to_owned(),
            score,
            rank: 1,
            flags: crate::projections::DeclarationFlags {
                is_private: false,
                is_generated: false,
                is_internal: false,
            },
        }
    }

    fn search_result(rows: Vec<DeclarationSummary>) -> DeclarationSearchResult {
        let n = rows.len();
        DeclarationSearchResult {
            declarations: rows,
            truncated: false,
            facts: DeclarationSearchFacts {
                declarations_scanned: n,
                after_name_filter: n,
                after_kind_filter: n,
                after_required_constants_filter: n,
                after_conclusion_filter: n,
                after_scope_filter: n,
                source_lookups: 0,
                broad_pruning: Vec::new(),
                truncated: false,
                timings: crate::projections::DeclarationSearchTimings {
                    scan_micros: 0,
                    rank_micros: 0,
                    source_micros: 0,
                },
            },
        }
    }

    #[test]
    fn search_for_proof_request_defaults() {
        let req: SearchForProofRequest = serde_json::from_value(json!({"goal":"⊢ True"})).unwrap();
        assert_eq!(req.mode.unwrap_or_default(), ProofSearchMode::NextStep);
        assert!(req.file.is_none());
        assert_eq!(req.goal.as_deref(), Some("⊢ True"));
    }

    #[test]
    fn extract_heads_recognizes_relational_membership_and_divisibility() {
        assert!(extract_heads("⊢ a ≤ b").iter().any(|head| head == "LE.le"));
        assert!(extract_heads("⊢ x ∈ s").iter().any(|head| head == "Membership.mem"));
        assert!(extract_heads("⊢ a ∣ b").iter().any(|head| head == "Dvd.dvd"));
        assert!(extract_heads("⊢ s ⊆ t").iter().any(|head| head == "HasSubset.Subset"));
        assert!(extract_heads("⊢ a ≠ b").iter().any(|head| head == "Ne"));
        assert!(extract_heads("⊢ a < b").iter().any(|head| head == "LT.lt"));
        // `≤` and `<=` are LE.le, never LT.lt; an ASCII arrow is never GT.gt.
        assert!(!extract_heads("⊢ a ≤ b").iter().any(|head| head == "LT.lt"));
        assert!(extract_heads("⊢ a <= b").iter().any(|head| head == "LE.le"));
        assert!(!extract_heads("f : a -> b").iter().any(|head| head == "GT.gt"));
        // The plain-Eq case is unchanged.
        assert!(extract_heads("⊢ a = b").iter().any(|head| head == "Eq"));
    }

    #[test]
    fn plan_searches_emits_conclusion_head_for_le_goal() {
        let profile = profile_from_text(
            "⊢ I ≤ I.saturatedClosure".to_owned(),
            Some("CategoryTheory".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert!(profile.heads.iter().any(|head| head == "LE.le"));
        let searches = plan_searches(&profile, ProofSearchMode::NextStep);
        let head = searches
            .iter()
            .find(|search| search.label == "conclusion_head")
            .expect("a relational goal must trigger a conclusion_head search");
        assert_eq!(head.request.conclusion_head.as_deref(), Some("LE.le"));
    }

    #[test]
    fn plan_searches_keeps_eq_head_for_eq_goal_with_constants() {
        let profile = profile_from_text(
            "⊢ Nat.succ n = Nat.succ m".to_owned(),
            Some("Nat".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        let searches = plan_searches(&profile, ProofSearchMode::Exact);
        // The Eq head search still runs; a broad head carries a distinct label so
        // it does not falsely corroborate generic equality lemmas.
        let head = searches
            .iter()
            .find(|search| search.request.conclusion_head.as_deref() == Some("Eq"))
            .expect("the plain-Eq head path must still run");
        assert_eq!(head.label, "broad_conclusion_head");
    }

    #[test]
    fn preferred_head_picks_specific_over_broad() {
        // A hypothesis `=` plus a goal `≤`: the specific LE.le head must win.
        let heads = extract_heads("h : x = y\n⊢ x ≤ y");
        assert_eq!(
            preferred_head(&heads, ProofSearchMode::NextStep).as_deref(),
            Some("LE.le")
        );
        // A pure-Eq goal still yields Eq.
        let eq_heads = extract_heads("⊢ x = y");
        assert_eq!(
            preferred_head(&eq_heads, ProofSearchMode::NextStep).as_deref(),
            Some("Eq")
        );
    }

    #[test]
    fn lexical_only_candidates_are_demoted_below_corroborated() {
        let profile = profile_from_text(
            "⊢ I ≤ I.saturatedClosure".to_owned(),
            None,
            Vec::new(),
            "context".to_owned(),
            None,
            Vec::new(),
        );
        // A genuine LE.le head match (low base score) and a high-scoring lexical
        // suffix hit that no head/constant search corroborates.
        let head_row = summary(
            "CategoryTheory.MorphismProperty.le_saturatedClosure",
            Some("Mathlib.CategoryTheory.MorphismProperty.Basic"),
            60,
        );
        let noise_row = summary("Rat.den_intCast", Some("Mathlib.Data.Rat.Cast"), 120);
        let result = rank_results(
            &profile,
            ProofSearchMode::NextStep,
            10,
            2,
            vec![
                ("conclusion_head", search_result(vec![head_row])),
                ("name_fragment", search_result(vec![noise_row])),
            ],
            Vec::new(),
            false,
        );
        assert_eq!(
            result.candidates.first().expect("a candidate").name,
            "CategoryTheory.MorphismProperty.le_saturatedClosure",
            "the head-corroborated candidate must rank above the higher-scoring lexical hit"
        );
        assert_eq!(result.candidates.last().expect("a candidate").name, "Rat.den_intCast");
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning.contains("lexical guesses")),
            "a head-corroborated result set is not lexical-only"
        );
    }

    #[test]
    fn all_lexical_candidates_trigger_lexical_only_warning() {
        let profile = profile_from_text(
            "⊢ I ≤ I.saturatedClosure".to_owned(),
            None,
            Vec::new(),
            "context".to_owned(),
            None,
            Vec::new(),
        );
        // Only a name-fragment hit (matches the `le` fragment), nothing corroborates it.
        let row = summary("Rat.le_intCast", Some("Mathlib.Data.Rat.Cast"), 100);
        let result = rank_results(
            &profile,
            ProofSearchMode::NextStep,
            10,
            1,
            vec![("name_fragment", search_result(vec![row]))],
            Vec::new(),
            false,
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("lexical guesses")),
            "an entirely uncorroborated result set must carry the lexical-only warning"
        );
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
        let result = empty_result(profile, Vec::new(), false);
        assert_eq!(result.diagnostics.returned_count, 0);
        assert!(result.diagnostics.funnel.is_none(), "quiet omits the search funnel");
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
        assert_eq!(profile.kind, GoalProfileKind::Generic);
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
        assert_eq!(profile.kind, GoalProfileKind::RatArithmetic);
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
        let generic = summary("Acc.ndrecOn_eq_ndrecOnC", None, 100);
        let rat_candidate = summary("Rat.num_den_helper", Some("Mathlib.Data.Rat.Lemmas"), 100);
        assert!(
            candidate_score(&rat_candidate, &profile, ProofSearchMode::NextStep, "name_fragment")
                > candidate_score(&generic, &profile, ProofSearchMode::NextStep, "name_fragment")
        );
    }

    #[test]
    fn int_factorization_profile_prefers_topical_candidates_over_cast_noise() {
        let profile = profile_from_text(
            "n : ℤ\nh : n.factorization p ≠ 0\n⊢ p ∣ n".to_owned(),
            Some("Int".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert_eq!(profile.kind, GoalProfileKind::IntFactorization);
        assert!(
            profile
                .name_fragments
                .iter()
                .any(|fragment| fragment == "factorization")
        );
        let topical = summary(
            "Int.factorization_dvd_of_mem_support",
            Some("Mathlib.Data.Int.Factorization"),
            90,
        );
        let generic_cast = summary("Int.cast_add", Some("Mathlib.Data.Int.Cast"), 100);
        assert!(
            candidate_score(&topical, &profile, ProofSearchMode::NextStep, "name_fragment")
                > candidate_score(&generic_cast, &profile, ProofSearchMode::NextStep, "name_fragment")
        );
    }

    #[test]
    fn model_theory_relabel_profile_prefers_formula_candidates_over_array_noise() {
        let profile = profile_from_text(
            "φ : FirstOrder.Language.BoundedFormula L ν n\n⊢ φ.relabel σ = ψ".to_owned(),
            Some("FirstOrder".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert_eq!(profile.kind, GoalProfileKind::ModelTheoryRelabel);
        assert!(profile.name_fragments.iter().any(|fragment| fragment == "relabel"));
        let topical = summary(
            "FirstOrder.Language.BoundedFormula.relabel_id",
            Some("Mathlib.ModelTheory.Syntax"),
            90,
        );
        let array_noise = summary("Array.map_eq_map_data", Some("Init.Data.Array"), 100);
        assert!(
            candidate_score(&topical, &profile, ProofSearchMode::NextStep, "name_fragment")
                > candidate_score(&array_noise, &profile, ProofSearchMode::NextStep, "name_fragment")
        );
    }

    #[test]
    fn linear_profile_allows_int_solver_candidates_but_factorization_does_not() {
        let linear = profile_from_text(
            "x y : ℤ\nh : x + y ≤ 3\n⊢ x ≤ 3".to_owned(),
            None,
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert_eq!(linear.kind, GoalProfileKind::LinearArithmetic);
        let factorization = profile_from_text(
            "h : n.factorization p ≠ 0\n⊢ p ∣ n".to_owned(),
            None,
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        let solver = summary("Int.Linear.cooper_dvd", Some("Mathlib.Tactic.Omega"), 100);
        assert!(
            candidate_score(&solver, &linear, ProofSearchMode::NextStep, "name_fragment")
                > candidate_score(&solver, &factorization, ProofSearchMode::NextStep, "name_fragment")
        );
    }

    #[test]
    fn linear_profile_prefers_solver_and_int_inequality_candidates_over_lt_noise() {
        let profile = profile_from_text(
            "x y : ℤ\nhxy : x + 2 * y ≤ 17\nhy : 0 ≤ y\n⊢ x ≤ 17".to_owned(),
            Some("Int".to_owned()),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
        );
        assert_eq!(profile.kind, GoalProfileKind::LinearArithmetic);

        let solver = summary(
            "Int.Linear.ExprCnstr.denote_le",
            Some("Mathlib.Tactic.Omega.IntList"),
            80,
        );
        let cooper = summary(
            "Int.Cooper.proof_of_linear_combination",
            Some("Mathlib.Tactic.Omega"),
            80,
        );
        let array_noise = summary("Array.getElem?_of_lt", Some("Init.Data.Array.Basic"), 100);
        let order_noise = summary("Antitone.reflect_lt", Some("Mathlib.Order.Basic"), 100);

        let solver_score = candidate_score(&solver, &profile, ProofSearchMode::NextStep, "name_fragment");
        let cooper_score = candidate_score(&cooper, &profile, ProofSearchMode::NextStep, "name_fragment");
        let array_score = candidate_score(&array_noise, &profile, ProofSearchMode::NextStep, "name_fragment");
        let order_score = candidate_score(&order_noise, &profile, ProofSearchMode::NextStep, "name_fragment");

        assert!(solver_score > array_score, "{solver_score} should beat {array_score}");
        assert!(cooper_score > order_score, "{cooper_score} should beat {order_score}");
    }
}
