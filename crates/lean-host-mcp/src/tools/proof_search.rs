//! Proof-agent declaration retrieval.
//!
//! `search_for_proof` is an intent-shaped orchestration over existing
//! bounded primitives: file-backed goal features come from the supervised Lean
//! frontend, explicit goal text uses declaration search, and search's
//! `merge_hits` puts the two kinds of hit in one order without a score. It
//! deliberately does not render declarations.

// Tool handlers consume owned requests so worker calls can cross async
// boundaries without borrow plumbing.
#![allow(clippy::needless_pass_by_value)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use lean_rs_worker_parent::{LeanWorkerDeclarationFilter, LeanWorkerDeclarationNameMatch, LeanWorkerDeclarationSearch};
use lean_semantic_search_contract::{ProofGoalFeatureRequest, SourcePosition};
use lean_semantic_search_retrieval::{LexicalHit, MergedHit, NameMatch, merge_hits};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::{BrokerCall, ProjectHint};
use crate::envelope::{Response, RuntimeFacts};
use crate::error::Result;
use crate::projections::{
    DeclarationNameMatch, DeclarationSearchFacts, DeclarationSearchResult, DeclarationSummary, SourceRange,
    project_declaration_search,
};
use crate::semantic_search::{SemanticProofCandidate, SemanticProofSearchRequest, SemanticProofSearchResult};
use crate::tools::position::ProofPositionSelector;
use crate::tools::{ToolContext, session_imports};
use crate::trust::ArtifactTrust;

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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
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
    /// Best first: declarations whose full name is a searched fragment, then
    /// semantic hits in retrieval's order, then the other lexical hits.
    pub candidates: Vec<ProofSearchCandidate>,
    pub diagnostics: ProofSearchDiagnostics,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct TargetProfile {
    source_declaration: Option<String>,
    constants: Vec<String>,
    heads: Vec<String>,
    name_fragments: Vec<String>,
    imports: Vec<String>,
    proof_state_status: String,
    cache_status: Option<String>,
    warnings: Vec<String>,
    semantic: Option<SemanticProofSearchRequest>,
    /// The source file the profile read, with the digest of its bytes, for
    /// the reply's trust facts. `None` for a search from explicit text.
    source_fact: Option<ArtifactTrust>,
}

/// The kind of goal, which picks the topical name fragments to search for.
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

impl PlannedSearch {
    /// Whether the search filters on a broad conclusion head alone. `Eq`,
    /// `Iff`, `Exists` and `True` head nearly every statement, so such a search
    /// returns the first theorems by name, whatever they say.
    fn filters_on_broad_head_alone(&self) -> bool {
        self.request.name_fragment.is_none()
            && self.request.required_constants.is_empty()
            && self.request.conclusion_head.as_deref().is_some_and(is_broad_head)
    }
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
    let mut profile = target_profile(ctx, req, limit)?;
    let mut warnings = std::mem::take(&mut profile.warnings);
    let source_fact = profile.source_fact.take();
    let searches = plan_searches(&profile, mode);
    let semantic = run_semantic_search(ctx, project.clone(), &profile).await?;

    if searches.is_empty() {
        if semantic.is_none() {
            warnings.push("no usable goal constants, heads, or name fragments were available for retrieval".to_owned());
        }
        let semantic = semantic.map(|call| call.value);
        let result = if semantic.is_some() {
            rank_results(&profile, mode, limit, 1, semantic, Vec::new(), warnings, full)
        } else {
            empty_result(profile, warnings, full)
        };
        let hint = ProjectHint::from_request(project);
        let identity = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
        return Ok(Response::ok(result, identity.freshness)
            .with_runtime(identity.runtime)
            .with_trust_artifacts(source_fact));
    }

    // One call for the whole group: the searches share an import set, so they
    // share a session, and issuing them separately paid a session open — and
    // therefore a full re-import — per search.
    let group: Vec<_> = searches.iter().take(MAX_SEARCHES).collect();
    let call = match run_declaration_searches(
        ctx,
        project.clone(),
        profile.imports.clone(),
        group.iter().map(|search| search.request.clone()).collect(),
    )
    .await
    {
        Ok(call) => call,
        Err(err) if crate::diagnosis::missing_olean_failure(&err) => {
            let result = empty_result(profile.clone(), warnings, full);
            let hint = ProjectHint::from_request(project);
            let identity = ctx
                .broker
                .project_identity_without_worker(&hint, profile.imports.clone())?;
            let response = Response::ok(result, identity.freshness)
                .with_runtime(identity.runtime)
                .with_trust_artifacts(source_fact);
            let response =
                crate::diagnosis::warn_needs_build(response, &crate::diagnosis::IncompleteCause::from_error(&err));
            return Ok(response.hint(
                "supply a valid file or explicit imports; search_for_proof will not broad-import Mathlib as fallback",
            ));
        }
        Err(err) => return Err(err),
    };
    // The search group's facts, not the semantic call's: it is the later of the
    // two and the one whose worker generation the response describes.
    let runtime: RuntimeFacts = call.runtime;
    // Results come back in request order, so labels re-pair positionally.
    let search_results: Vec<_> = group.iter().map(|search| search.label).zip(call.value).collect();

    let search_count = search_results.len().saturating_add(usize::from(semantic.is_some()));
    let semantic = semantic.map(|call| call.value);
    let result = rank_results(
        &profile,
        mode,
        limit,
        search_count,
        semantic,
        search_results,
        warnings,
        full,
    );
    let freshness_imports = profile.imports.clone();
    let hint = ProjectHint::from_request(project);
    let identity = ctx.broker.project_identity_without_worker(&hint, freshness_imports)?;
    Ok(Response::ok(result, identity.freshness)
        .with_runtime(runtime)
        .with_trust_artifacts(source_fact))
}

fn target_profile(ctx: &ToolContext, req: SearchForProofRequest, limit: usize) -> Result<TargetProfile> {
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
        let hint = ProjectHint::from_request(req.project.clone());
        let root = ctx.broker.resolve(&hint)?;
        let input = crate::tools::source_input::read_query_file(&root, &file)?;
        return Ok(source_profile(
            &root,
            &input,
            &declaration,
            &req.proof_position,
            &req.imports,
            explicit_text,
            limit,
        ));
    }

    Ok(profile_from_text(
        explicit_text,
        req.imports,
        "explicit_text".to_owned(),
        None,
        Vec::new(),
        None,
    ))
}

fn source_profile(
    root: &std::path::Path,
    input: &crate::tools::source_input::QueryFile,
    declaration: &str,
    position: &ProofPositionSelector,
    requested_imports: &[String],
    explicit_text: String,
    limit: usize,
) -> TargetProfile {
    let mut imports = input.imports.clone();
    for module in requested_imports {
        if !imports.contains(module) {
            imports.push(module.clone());
        }
    }
    let semantic = semantic_request_for_source(input, declaration, position, None, &imports, limit);
    let mut profile = profile_from_text(
        explicit_text,
        imports,
        "semantic_frontend".into(),
        None,
        Vec::new(),
        Some(semantic),
    );
    profile.source_declaration = Some(declaration.to_owned());
    profile.source_fact = Some(ArtifactTrust::source_file_edit_fresh(root, &input.resolved, input.hash));
    profile
}

fn profile_from_text(
    text: String,
    imports: Vec<String>,
    proof_state_status: String,
    cache_status: Option<String>,
    warnings: Vec<String>,
    semantic: Option<SemanticProofSearchRequest>,
) -> TargetProfile {
    let constants = extract_constants(&text);
    let heads = extract_heads(&text);
    let kind = classify_goal_profile(&text, &constants);
    let name_fragments = extract_name_fragments(&text, &constants, &heads, kind);
    TargetProfile {
        source_declaration: None,
        constants,
        heads,
        name_fragments,
        imports,
        proof_state_status,
        cache_status,
        warnings,
        semantic,
        source_fact: None,
    }
}

/// The semantic search for the goal at `proof_position` in `input`. The
/// goal's module stays empty: the broker sets it to the module Lake names for
/// the file and puts that module first among the candidates.
fn semantic_request_for_source(
    input: &crate::tools::source_input::QueryFile,
    declaration: &str,
    proof_position: &ProofPositionSelector,
    namespace: Option<String>,
    imports: &[String],
    limit: usize,
) -> SemanticProofSearchRequest {
    let mut candidate_modules: Vec<String> = Vec::new();
    for candidate in imports.iter().chain(input.imports.iter()) {
        if !candidate_modules.contains(candidate) {
            candidate_modules.push(candidate.clone());
        }
    }
    SemanticProofSearchRequest {
        goal: ProofGoalFeatureRequest {
            module: String::new(),
            source_text: input.source.clone(),
            file_label: Some(input.resolved.to_string_lossy().into_owned()),
            declaration: Some(short_declaration_name(declaration).to_owned()),
            position: source_position_for_selector(proof_position, &input.source),
            namespace,
        },
        candidate_modules,
        limit: limit.saturating_mul(3).clamp(10, 60),
    }
}

fn short_declaration_name(declaration: &str) -> &str {
    declaration.rsplit('.').next().unwrap_or(declaration)
}

fn source_position_for_selector(selector: &ProofPositionSelector, source: &str) -> Option<SourcePosition> {
    match selector {
        ProofPositionSelector::Default | ProofPositionSelector::Index { .. } => None,
        ProofPositionSelector::AfterText { text, occurrence } => {
            let occurrence = occurrence.unwrap_or(0);
            let byte_offset = nth_match(source, text, occurrence)?;
            let prefix = source.get(..byte_offset)?;
            let line = u32::try_from(prefix.bytes().filter(|byte| *byte == b'\n').count().saturating_add(1)).ok()?;
            let column_start = prefix.rfind('\n').map_or(0, |index| index.saturating_add(1));
            let column = u32::try_from(byte_offset.saturating_sub(column_start).saturating_add(1)).ok()?;
            Some(SourcePosition { line, column })
        }
    }
}

fn nth_match(source: &str, needle: &str, occurrence: u32) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut offset = 0usize;
    for seen in 0..=occurrence {
        let rest = source.get(offset..)?;
        let found = rest.find(needle)?;
        let absolute = offset.saturating_add(found);
        if seen == occurrence {
            return Some(absolute);
        }
        offset = absolute.saturating_add(needle.len());
    }
    None
}

async fn run_semantic_search(
    ctx: &ToolContext,
    project: Option<String>,
    profile: &TargetProfile,
) -> Result<Option<BrokerCall<SemanticProofSearchResult>>> {
    let Some(request) = profile.semantic.clone() else {
        return Ok(None);
    };
    let hint = ProjectHint::from_request(project);
    let call = ctx
        .broker
        .semantic_proof_search(hint, profile.imports.clone(), request)
        .await?;
    Ok(Some(call))
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
            push_head_search(&mut out, "rewrite_eq", Some("Eq"));
            push_head_search(&mut out, "rewrite_iff", Some("Iff"));
            push_fragment_searches(&mut out, &["rw", "rewrite"]);
        }
        ProofSearchMode::Simp => {
            push_fragment_searches(&mut out, &["simp"]);
            if let Some(head) = primary_head.as_deref() {
                push_head_search(&mut out, "simp_head", Some(head));
            }
        }
        ProofSearchMode::Exact | ProofSearchMode::Apply | ProofSearchMode::NextStep => {
            if let Some(head) = primary_head.as_deref()
                && !(mode == ProofSearchMode::NextStep && constants.is_empty() && is_broad_head(head))
            {
                // A specific relational head (`LE.le`, `Membership.mem`, …) is a
                // meaningful filter and labels the search `conclusion_head`. A
                // broad head (`Eq`/`Iff`/…) matches nearly everything, so it gets
                // a label of its own.
                let label = if is_broad_head(head) {
                    "broad_conclusion_head"
                } else {
                    "conclusion_head"
                };
                push_head_search(&mut out, label, Some(head));
            }
            if !constants.is_empty() {
                push_required_search(&mut out, "required_constants", constants);
            }
        }
    }

    for fragment in &profile.name_fragments {
        push_name_search(&mut out, "name_fragment", fragment);
    }

    let mut searches = dedupe_searches(out);
    // The order of the searches is the order `merge_hits` trusts their hits
    // in. A search on a broad head alone returns theorems by name, whatever
    // they say, so its hits go last. The sort is stable, so the rest keep the
    // order above.
    searches.sort_by_key(PlannedSearch::filters_on_broad_head_alone);
    searches
}

fn base_search(label: &'static str) -> PlannedSearch {
    PlannedSearch {
        label,
        request: LeanWorkerDeclarationSearch {
            name_fragment: None,
            name_match: LeanWorkerDeclarationNameMatch::Contains,
            kind: Some("theorem".to_owned()),
            required_constants: Vec::new(),
            conclusion_head: None,
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

fn push_head_search(out: &mut Vec<PlannedSearch>, label: &'static str, head: Option<&str>) {
    let Some(head) = head else {
        return;
    };
    let mut search = base_search(label);
    search.request.conclusion_head = Some(head.to_owned());
    out.push(search);
}

fn push_required_search(out: &mut Vec<PlannedSearch>, label: &'static str, constants: Vec<String>) {
    let mut search = base_search(label);
    search.request.required_constants = constants;
    out.push(search);
}

fn push_name_search(out: &mut Vec<PlannedSearch>, label: &'static str, fragment: &str) {
    let mut search = base_search(label);
    search.request.name_fragment = Some(fragment.to_owned());
    out.push(search);
}

fn push_fragment_searches(out: &mut Vec<PlannedSearch>, fragments: &[&str]) {
    for fragment in fragments {
        push_name_search(out, "mode_fragment", fragment);
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

/// Run a group of searches against one session; results keep request order.
async fn run_declaration_searches(
    ctx: &ToolContext,
    project: Option<String>,
    imports: Vec<String>,
    searches: Vec<LeanWorkerDeclarationSearch>,
) -> Result<BrokerCall<Vec<DeclarationSearchResult>>> {
    let hint = ProjectHint::from_request(project);
    // Uncached on purpose: a search names an environment, not a source file, so
    // there is no content hash to invalidate the answer. See the module doc of
    // `crate::cache`.
    let call = ctx
        .broker
        .search_declarations(hint, session_imports(imports.clone()), imports, searches)
        .await?;
    Ok(BrokerCall {
        value: call.value.into_iter().map(project_declaration_search).collect(),
        runtime: call.runtime,
        freshness: call.freshness,
    })
}

fn rank_results(
    profile: &TargetProfile,
    mode: ProofSearchMode,
    limit: usize,
    search_count: usize,
    semantic: Option<SemanticProofSearchResult>,
    search_results: Vec<(&'static str, DeclarationSearchResult)>,
    warnings: Vec<String>,
    full: bool,
) -> SearchForProofResult {
    let mut generated_count = 0usize;
    let mut pruned_count = 0usize;
    let mut search_truncated = false;
    let mut broad_pruning = Vec::new();
    let mut warnings = warnings;

    // Retrieval decides which matches admit a candidate, and its order is its
    // ranking. Keep every candidate it returns, in that order.
    let semantic = match semantic {
        Some(semantic) => {
            generated_count = generated_count.saturating_add(semantic.declaration_rows);
            for diagnostic in semantic.diagnostics {
                warnings.push(format!("semantic proof search: {diagnostic}"));
            }
            if semantic.goal_rows == 0 {
                warnings.push("semantic proof search produced no proof-goal features".to_owned());
            }
            semantic.candidates
        }
        None => Vec::new(),
    };

    // Each search's rows in the worker's order, the searches in the order
    // `plan_searches` trusts them.
    let mut lexical = Vec::new();
    for (label, result) in search_results {
        generated_count = generated_count.saturating_add(result.facts.after_conclusion_filter);
        pruned_count = pruned_count.saturating_add(pruned_from_facts(&result.facts, result.declarations.len()));
        search_truncated |= result.truncated || result.facts.truncated;
        for pruning in &result.facts.broad_pruning {
            broad_pruning.push(format!(
                "{}:{}:{}:{}",
                label, pruning.stage, pruning.reason, pruning.count
            ));
        }
        lexical.extend(result.declarations.into_iter().map(|row| (label, row)));
    }

    let lexical_hits = lexical
        .iter()
        .map(|(_, row)| LexicalHit {
            name: row.name.clone(),
            name_match: row.name_match.map(search_name_match),
        })
        .collect::<Vec<_>>();
    let semantic_names = semantic
        .iter()
        .map(|candidate| candidate.name.clone())
        .collect::<Vec<_>>();
    let mut merged = merge_hits(&lexical_hits, &semantic_names);
    if let Some(name) = &profile.source_declaration {
        merged.retain(|hit| &hit.name != name);
    }
    let ranked_count = merged.len();
    let candidates = merged
        .into_iter()
        .filter_map(|hit| proof_search_candidate(mode, hit, &lexical, &semantic))
        .take(limit)
        .collect::<Vec<_>>();

    // Honesty signal: if the goal carried domain name-fragments but no returned
    // candidate's name matches any of them, the results are conclusion-head
    // matches (e.g. generic `Iff`/`eq` lemmas), not domain-relevant. Say so
    // rather than letting the agent mistake noise for guidance.
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
            !candidate.match_reason.contains("semantic:")
                && !candidate.match_reason.contains("conclusion_head")
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

const fn search_name_match(name_match: DeclarationNameMatch) -> NameMatch {
    match name_match {
        DeclarationNameMatch::Exact => NameMatch::Exact,
        DeclarationNameMatch::Prefix => NameMatch::Prefix,
        DeclarationNameMatch::Substring => NameMatch::Substring,
    }
}

/// The candidate for one merged hit, with the evidence of every search and of
/// retrieval that found it. Its data come from its first lexical row, which
/// carries a source range, or else from its semantic candidate.
fn proof_search_candidate(
    mode: ProofSearchMode,
    hit: MergedHit,
    lexical: &[(&'static str, DeclarationSummary)],
    semantic: &[SemanticProofCandidate],
) -> Option<ProofSearchCandidate> {
    let mut reasons = BTreeSet::new();
    let mut first_row = None;
    for (label, row) in hit.lexical.iter().filter_map(|&position| lexical.get(position)) {
        reasons.insert((*label).to_owned());
        reasons.insert(row.match_reason.clone());
        first_row = first_row.or(Some(row));
    }
    let semantic = hit.semantic.and_then(|position| semantic.get(position));
    if let Some(candidate) = semantic {
        reasons.insert("semantic".to_owned());
        reasons.extend(candidate.evidence.iter().cloned());
    }
    let (kind, module, source) = match (first_row, semantic) {
        (Some(row), _) => (row.kind.clone(), row.module.clone(), row.source.clone()),
        (None, Some(candidate)) => (candidate.kind.as_str().to_owned(), Some(candidate.module.clone()), None),
        (None, None) => return None,
    };
    Some(ProofSearchCandidate {
        suggested_snippet: suggested_snippet(mode, &hit.name, &kind),
        name: hit.name,
        kind,
        module,
        source,
        match_reason: reasons.into_iter().collect::<Vec<_>>().join(","),
    })
}

fn is_broad_head(head: &str) -> bool {
    matches!(head, "Eq" | "Iff" | "Exists" | "True")
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

fn pruned_from_facts(facts: &DeclarationSearchFacts, returned: usize) -> usize {
    let limit_pruned = facts.after_conclusion_filter.saturating_sub(returned);
    facts
        .broad_pruning
        .iter()
        .fold(limit_pruned, |acc, pruning| acc.saturating_add(pruning.count))
}

fn suggested_snippet(mode: ProofSearchMode, name: &str, kind: &str) -> Option<String> {
    match mode {
        ProofSearchMode::Exact if kind == "theorem" => Some(format!("exact {name}")),
        ProofSearchMode::Apply if kind == "theorem" => Some(format!("apply {name}")),
        ProofSearchMode::Rewrite if kind == "theorem" => Some(format!("rw [{name}]")),
        ProofSearchMode::Simp if kind == "theorem" => Some(format!("simp [{name}]")),
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

    fn summary(name: &str, module: Option<&str>, name_match: Option<DeclarationNameMatch>) -> DeclarationSummary {
        DeclarationSummary {
            name: name.to_owned(),
            kind: "theorem".to_owned(),
            module: module.map(ToOwned::to_owned),
            source: None,
            match_reason: "name fragment".to_owned(),
            name_match,
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
                source_lookups: 0,
                broad_pruning: Vec::new(),
                truncated: false,
                timings: crate::projections::DeclarationSearchTimings {
                    scan_micros: 0,
                    sort_micros: 0,
                    source_micros: 0,
                },
            },
        }
    }

    fn semantic_candidate(name: &str, evidence: &str) -> SemanticProofCandidate {
        SemanticProofCandidate {
            name: name.to_owned(),
            module: "Fixture".to_owned(),
            kind: lean_semantic_search_contract::DeclarationKind::Theorem,
            evidence: vec![evidence.to_owned()],
        }
    }

    fn semantic_result(candidates: Vec<SemanticProofCandidate>) -> SemanticProofSearchResult {
        SemanticProofSearchResult {
            declaration_rows: candidates.len(),
            candidates,
            diagnostics: Vec::new(),
            goal_rows: 1,
        }
    }

    fn profile(text: &str) -> TargetProfile {
        profile_from_text(
            text.to_owned(),
            Vec::new(),
            "context".to_owned(),
            None,
            Vec::new(),
            None,
        )
    }

    fn names(result: &SearchForProofResult) -> Vec<&str> {
        result
            .candidates
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect()
    }

    fn kind_of(text: &str) -> GoalProfileKind {
        classify_goal_profile(text, &extract_constants(text))
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
        let profile = profile("⊢ I ≤ I.saturatedClosure");
        assert!(profile.heads.iter().any(|head| head == "LE.le"));
        let searches = plan_searches(&profile, ProofSearchMode::NextStep);
        let head = searches
            .iter()
            .find(|search| search.label == "conclusion_head")
            .expect("a relational goal must trigger a conclusion_head search");
        assert_eq!(head.request.conclusion_head.as_deref(), Some("LE.le"));
    }

    /// The `Eq` head search still runs, under a label of its own, after every
    /// search whose filter says more about the goal.
    #[test]
    fn plan_searches_runs_a_broad_head_search_last() {
        let searches = plan_searches(&profile("⊢ Nat.succ n = Nat.succ m"), ProofSearchMode::Exact);
        let labels = searches.iter().map(|search| search.label).collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "required_constants",
                "name_fragment",
                "name_fragment",
                "broad_conclusion_head"
            ]
        );
        let head = searches.last().expect("a search");
        assert_eq!(head.request.conclusion_head.as_deref(), Some("Eq"));
    }

    #[test]
    fn plan_searches_runs_rewrite_head_searches_after_its_name_searches() {
        let labels = plan_searches(&profile("⊢ a + b = b + a"), ProofSearchMode::Rewrite)
            .iter()
            .map(|search| search.label)
            .collect::<Vec<_>>();
        assert_eq!(labels, ["mode_fragment", "mode_fragment", "rewrite_eq", "rewrite_iff"]);
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

    /// The order is search's merge rule: exact names, then semantic hits, then
    /// the other lexical hits in the order of their searches.
    #[test]
    fn candidates_come_exact_names_first_then_semantic_hits_then_other_lexical_hits() {
        let semantic = semantic_result(vec![semantic_candidate("Fixture.semantic_hit", "semantic:role_head:1")]);
        let result = rank_results(
            &profile("⊢ x ≤ y"),
            ProofSearchMode::Exact,
            10,
            3,
            Some(semantic),
            vec![
                (
                    "conclusion_head",
                    search_result(vec![summary("Fixture.head_hit", None, None)]),
                ),
                (
                    "name_fragment",
                    search_result(vec![
                        summary("Fixture.le", None, Some(DeclarationNameMatch::Exact)),
                        summary("Fixture.le_refl", None, Some(DeclarationNameMatch::Prefix)),
                    ]),
                ),
            ],
            Vec::new(),
            false,
        );
        assert_eq!(
            names(&result),
            [
                "Fixture.le",
                "Fixture.semantic_hit",
                "Fixture.head_hit",
                "Fixture.le_refl"
            ]
        );
        let first = result.candidates.first().expect("a candidate");
        assert_eq!(first.suggested_snippet.as_deref(), Some("exact Fixture.le"));
    }

    /// A declaration two sources find appears once, with the evidence of both,
    /// and takes its data from the lexical row, which has a source range.
    #[test]
    fn a_declaration_found_twice_appears_once_with_all_its_evidence() {
        let semantic = semantic_result(vec![semantic_candidate(
            "Fixture.le_closure",
            "semantic:role_conclusion_const:2",
        )]);
        let mut row = summary("Fixture.le_closure", Some("Fixture.Closure"), None);
        row.source = Some(SourceRange {
            file: "Fixture/Closure.lean".to_owned(),
            start_line: 3,
            start_column: 0,
            end_line: 4,
            end_column: 0,
        });
        let result = rank_results(
            &profile("⊢ I ≤ I.saturatedClosure"),
            ProofSearchMode::NextStep,
            10,
            2,
            Some(semantic),
            vec![("conclusion_head", search_result(vec![row]))],
            Vec::new(),
            false,
        );
        let [candidate] = result.candidates.as_slice() else {
            panic!("expected one candidate, got {:?}", result.candidates);
        };
        for reason in ["conclusion_head", "semantic", "semantic:role_conclusion_const:2"] {
            assert!(
                candidate.match_reason.split(',').any(|part| part == reason),
                "{reason} missing from {}",
                candidate.match_reason
            );
        }
        assert_eq!(candidate.module.as_deref(), Some("Fixture.Closure"));
        assert!(candidate.source.is_some(), "the lexical row's source range is kept");
    }

    #[test]
    fn all_lexical_candidates_trigger_lexical_only_warning() {
        // Only a name-fragment hit (matches the `le` fragment), nothing else
        // vouches for it.
        let row = summary(
            "Rat.le_intCast",
            Some("Mathlib.Data.Rat.Cast"),
            Some(DeclarationNameMatch::Substring),
        );
        let result = rank_results(
            &profile("⊢ I ≤ I.saturatedClosure"),
            ProofSearchMode::NextStep,
            10,
            1,
            None,
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
    fn a_head_hit_clears_the_lexical_only_warning() {
        let result = rank_results(
            &profile("⊢ I ≤ I.saturatedClosure"),
            ProofSearchMode::NextStep,
            10,
            2,
            None,
            vec![
                (
                    "conclusion_head",
                    search_result(vec![summary(
                        "CategoryTheory.MorphismProperty.le_saturatedClosure",
                        Some("Mathlib.CategoryTheory.MorphismProperty.Basic"),
                        None,
                    )]),
                ),
                (
                    "name_fragment",
                    search_result(vec![summary(
                        "Rat.den_intCast",
                        Some("Mathlib.Data.Rat.Cast"),
                        Some(DeclarationNameMatch::Substring),
                    )]),
                ),
            ],
            Vec::new(),
            false,
        );
        assert_eq!(
            names(&result),
            ["CategoryTheory.MorphismProperty.le_saturatedClosure", "Rat.den_intCast"]
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning.contains("lexical guesses")),
            "a result set with a head match is not lexical-only"
        );
    }

    #[test]
    fn semantic_candidates_rank_ahead_of_lexical_fallback() {
        let lexical = summary(
            "Rat.intCast_num",
            Some("Mathlib.Data.Rat.Cast"),
            Some(DeclarationNameMatch::Prefix),
        );
        let semantic = semantic_result(vec![SemanticProofCandidate {
            name: "LeanRsFixture.ProofSearchFacts.cast_num_den_helper".to_owned(),
            module: "LeanRsFixture.ProofSearchFacts".to_owned(),
            kind: lean_semantic_search_contract::DeclarationKind::Theorem,
            evidence: vec!["semantic:role_conclusion_const:2".to_owned()],
        }]);
        let result = rank_results(
            &profile("⊢ ↑(c * q.num) = ↑m * q"),
            ProofSearchMode::NextStep,
            10,
            2,
            Some(semantic),
            vec![("name_fragment", search_result(vec![lexical]))],
            Vec::new(),
            false,
        );
        assert_eq!(
            names(&result),
            ["LeanRsFixture.ProofSearchFacts.cast_num_den_helper", "Rat.intCast_num"]
        );
        let first = result.candidates.first().expect("semantic candidate");
        assert!(first.match_reason.contains("semantic:role_conclusion_const"));
        assert_eq!(first.module.as_deref(), Some("LeanRsFixture.ProofSearchFacts"));
    }

    /// Retrieval's order is its ranking; a candidate whose only evidence is a
    /// head keeps its place.
    #[test]
    fn semantic_candidates_keep_retrievals_order() {
        let semantic = semantic_result(vec![
            semantic_candidate("Fixture.head_only", "semantic:role_head:1"),
            semantic_candidate("Fixture.shares_a_constant", "semantic:role_conclusion_const:2"),
        ]);
        let result = rank_results(
            &profile("⊢ x = y"),
            ProofSearchMode::NextStep,
            10,
            1,
            Some(semantic),
            Vec::new(),
            Vec::new(),
            true,
        );
        assert_eq!(names(&result), ["Fixture.head_only", "Fixture.shares_a_constant"]);
        let funnel = result.diagnostics.funnel.expect("full telemetry funnel");
        assert!(funnel.broad_pruning.is_empty(), "{:?}", funnel.broad_pruning);
    }

    #[test]
    fn the_goal_declaration_is_dropped_and_the_limit_cuts_from_the_front() {
        let mut profile = profile("⊢ x ≤ y");
        profile.source_declaration = Some("Fixture.goal".to_owned());
        let rows = ["Fixture.goal", "Fixture.first", "Fixture.second"]
            .into_iter()
            .map(|name| summary(name, None, Some(DeclarationNameMatch::Substring)))
            .collect();
        let result = rank_results(
            &profile,
            ProofSearchMode::NextStep,
            1,
            1,
            None,
            vec![("name_fragment", search_result(rows))],
            Vec::new(),
            true,
        );
        assert_eq!(names(&result), ["Fixture.first"]);
        let funnel = result.diagnostics.funnel.expect("full telemetry funnel");
        assert_eq!(funnel.ranked_count, 2);
    }

    #[test]
    fn limit_clamps_to_tool_cap() {
        let profile = profile_from_text(
            "⊢ True".to_owned(),
            Vec::new(),
            "explicit_text".to_owned(),
            None,
            Vec::new(),
            None,
        );
        let result = empty_result(profile, Vec::new(), false);
        assert_eq!(result.diagnostics.returned_count, 0);
        assert!(result.diagnostics.funnel.is_none(), "quiet omits the search funnel");
        assert_eq!(MAX_LIMIT, 20);
    }

    #[test]
    fn extracts_heads_and_fragments() {
        let text = "⊢ Nat.succ n = Nat.succ m";
        let profile = profile(text);
        assert!(profile.heads.iter().any(|head| head == "Eq"));
        assert!(profile.constants.iter().any(|constant| constant == "Nat.succ"));
        assert!(profile.name_fragments.iter().any(|fragment| fragment == "nat"));
        assert_eq!(kind_of(text), GoalProfileKind::Generic);
    }

    #[test]
    fn rat_arithmetic_profile_keeps_useful_lowercase_fragments() {
        let text = "q : ℚ\nm c : ℤ\nhc : m = ↑q.den * c\n⊢ ↑(c * q.num) = ↑m * q";
        let profile = profile(text);
        assert_eq!(kind_of(text), GoalProfileKind::RatArithmetic);
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

    /// The kind of goal picks the topical fragments the searches ask for.
    #[test]
    fn goal_kinds_choose_their_topical_fragments() {
        let factorization = "n : ℤ\nh : n.factorization p ≠ 0\n⊢ p ∣ n";
        assert_eq!(kind_of(factorization), GoalProfileKind::IntFactorization);
        assert!(
            profile(factorization)
                .name_fragments
                .iter()
                .any(|fragment| fragment == "factorization")
        );

        let relabel = "φ : FirstOrder.Language.BoundedFormula L ν n\n⊢ φ.relabel σ = ψ";
        assert_eq!(kind_of(relabel), GoalProfileKind::ModelTheoryRelabel);
        assert!(
            profile(relabel)
                .name_fragments
                .iter()
                .any(|fragment| fragment == "relabel")
        );

        let linear = "x y : ℤ\nh : x + y ≤ 3\n⊢ x ≤ 3";
        assert_eq!(kind_of(linear), GoalProfileKind::LinearArithmetic);
        assert!(
            profile(linear)
                .name_fragments
                .iter()
                .any(|fragment| fragment == "omega")
        );
    }

    #[test]
    fn a_search_from_a_file_reports_the_digest_of_the_text_it_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let text = "import Init\ntheorem demo : 1 + 1 = 2 := by\n  rfl\n";
        std::fs::write(root.join("Demo.lean"), text).unwrap();
        let input = crate::tools::source_input::read_query_file(&root, std::path::Path::new("Demo.lean")).unwrap();

        let profile = source_profile(
            &root,
            &input,
            "demo",
            &ProofPositionSelector::Default,
            &[],
            String::new(),
            DEFAULT_LIMIT,
        );

        // The broker names the goal's module once Lake has named it.
        let semantic = profile
            .semantic
            .as_ref()
            .expect("a search from a file asks the semantic search");
        assert_eq!(semantic.goal.module, "");
        assert_eq!(semantic.candidate_modules, vec!["Init".to_owned()]);

        let fact = profile.source_fact.expect("a search from a file names the file");
        assert_eq!(fact.path.as_deref(), Some("Demo.lean"));
        assert_eq!(fact.content_sha256, Some(crate::digest::sha256_hex(text.as_bytes())));
        let from_text = profile_from_text(String::new(), Vec::new(), String::new(), None, Vec::new(), None);
        assert!(from_text.source_fact.is_none());
    }
}
