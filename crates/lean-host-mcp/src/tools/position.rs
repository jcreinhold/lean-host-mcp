//! Bounded declaration-context and reference tools.
//!
//! The public proof-agent path uses declaration/proof-position anchors. Raw
//! cursor and span selectors remain worker internals, not MCP inputs.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationTargetInfo, LeanWorkerDeclarationTargetResult, LeanWorkerElabOptions, LeanWorkerLocalInfo,
    LeanWorkerModuleCacheStatus, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchItem,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryBatchResult, LeanWorkerModuleQueryCacheFacts,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQueryResult, LeanWorkerModuleQuerySelector,
    LeanWorkerModuleQueryTimings, LeanWorkerModuleSourceSpan, LeanWorkerNameRef, LeanWorkerOutputBudgets,
    LeanWorkerProofPositionSelector, LeanWorkerProofStateInfo, LeanWorkerProofStateResult, LeanWorkerRenderedInfo,
    LeanWorkerSurroundingDeclarationResult, LeanWorkerTypeAtResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::ProjectHint;
use crate::envelope::{Freshness, Response, RuntimeFacts};
use crate::error::{Result, ServerError};
use crate::projections::{Diagnostic, ElabFailure, Severity, project_failure};
use crate::tools::source_input::{header_imports, read_query_file, resolve_path};
use crate::tools::{ToolContext, session_imports};

/// Hard cap on project-wide reference aggregation. File-local reference
/// queries are also bounded by the upstream projection.
const MAX_REFERENCES: usize = 1000;

const PROOF_STATE_DIAGNOSTICS_ID: &str = "diagnostics";
const PROOF_STATE_CONTEXT_ID: &str = "proof_state";

const PROOF_AGENT_PER_FIELD_BYTES: u32 = 4 * 1024;
const EXPERT_QUERY_PER_FIELD_BYTES: u32 = 8 * 1024;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SourceSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenderedText {
    pub value: String,
    pub truncated: bool,
}

/// Per-severity counts attached to every diagnostics-bearing variant.
#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
pub struct DiagnosticSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
}

impl DiagnosticSummary {
    fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut summary = Self::default();
        for diagnostic in diagnostics {
            let bucket = match diagnostic.severity {
                Severity::Error => &mut summary.errors,
                Severity::Warning => &mut summary.warnings,
                Severity::Info => &mut summary.info,
            };
            *bucket = bucket.saturating_add(1);
        }
        summary
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticsBlock {
    pub summary: DiagnosticSummary,
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ModuleQueryTimings {
    pub header_import_micros: u64,
    pub elaboration_micros: u64,
    pub projection_micros: u64,
    pub rendering_micros: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ModuleQueryFacts {
    pub cache_status: &'static str,
    pub output_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_entry_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_approx_bytes: Option<u64>,
    pub timings: ModuleQueryTimings,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LocalInfo {
    pub name: String,
    pub binder_info: String,
    pub type_str: RenderedText,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<RenderedText>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeclarationTargetInfo {
    pub short_name: String,
    pub declaration_name: String,
    pub namespace_name: String,
    pub declaration_kind: String,
    pub declaration_span: SourceSpan,
    pub name_span: SourceSpan,
    pub body_span: SourceSpan,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeclarationTargetProjection {
    Target { info: DeclarationTargetInfo },
    NotFound,
    Ambiguous { candidates: Vec<DeclarationTargetInfo> },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SurroundingDeclarationProjection {
    Declaration { info: DeclarationTargetInfo },
    None,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProofStateContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration_name: Option<String>,
    pub namespace_name: String,
    pub goals_before: Vec<String>,
    pub goals_after: Vec<String>,
    pub locals: Vec<LocalInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<RenderedText>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProofStateProjection {
    State { info: Box<ProofStateContext> },
    Unavailable { message: String },
    Ambiguous { candidates: Vec<DeclarationTargetInfo> },
    NeedsBuild { missing: Vec<String> },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TypeAtProjection {
    Term {
        expr: RenderedText,
        type_str: RenderedText,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_type: Option<RenderedText>,
        span: SourceSpan,
    },
    NoTerm,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReferenceHit {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// `"def"` for binder occurrences, `"ref"` for use sites.
    pub kind: &'static str,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReferencesProjection {
    pub references: Vec<ReferenceHit>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProofStateRequest {
    /// Path to a `.lean` file. Resolved against the resolved project root
    /// if relative.
    pub file: PathBuf,
    /// Fully-qualified or file-local declaration name to inspect.
    pub declaration: String,
    #[serde(default)]
    pub proof_position: ProofPositionSelector,
    /// Optional explicit project root for this call.
    #[serde(default)]
    pub project: Option<String>,
}

/// Where in a proof to act.
///
/// `default` (also when the field is omitted) targets the **pristine entry
/// goal** — the proof state *before any tactic runs*. `proof_state` reports it
/// as `goals_before` (equal to `goals_after`, since nothing has run), and a
/// `try_proof_step` snippet is spliced *before* the first tactic, so a
/// from-scratch tactic block elaborates against this goal.
///
/// `index` selects the state *after* the Nth tactic has run (so `index: 0` is
/// the first-tactic state — read `goals_after` and continue from there);
/// `after_text` selects the state just after a matched source fragment.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProofPositionSelector {
    #[default]
    Default,
    Index {
        index: u32,
    },
    AfterText {
        text: String,
        /// Which match of `text` to use (0-based); defaults to the first.
        #[serde(default)]
        occurrence: Option<u32>,
    },
}

pub(crate) fn worker_proof_position(position: ProofPositionSelector) -> LeanWorkerProofPositionSelector {
    match position {
        // The default is the pristine entry goal: `proof_state`'s `goals_before`
        // shows it and a `try_proof_step` snippet splices before the first
        // tactic, so the two tools agree on where the default proof starts. The
        // old first-tactic-state default is still reachable as `Index { 0 }`.
        ProofPositionSelector::Default => LeanWorkerProofPositionSelector::Entry,
        ProofPositionSelector::Index { index } => LeanWorkerProofPositionSelector::Index { index },
        ProofPositionSelector::AfterText { text, occurrence } => {
            LeanWorkerProofPositionSelector::AfterText { text, occurrence }
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SelectorMessage {
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProofStateResult {
    Context {
        diagnostics: DiagnosticsBlock,
        #[serde(skip_serializing_if = "Option::is_none")]
        declaration_name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace_name: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        goals_before: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        goals_after: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        locals: Vec<LocalInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_type: Option<RenderedText>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        total_truncated: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unavailable: Vec<SelectorMessage>,
        /// Selectors that could not resolve because the project environment is
        /// incomplete. Separated from `unavailable` so the agent sees a
        /// `lake build` cue rather than a generic "unavailable"; the envelope
        /// also carries the canonical warning.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        needs_build: Vec<SelectorMessage>,
        /// Competing declarations when the requested name is genuinely
        /// ambiguous. Empty otherwise; the envelope warning names them too.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        ambiguous: Vec<DeclarationTargetInfo>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        budget_exceeded: Vec<SelectorMessage>,
        /// Worker-query telemetry (cache status, byte counts, timings). Pure
        /// operational signal; emitted only under `telemetry.verbosity = full`.
        #[serde(skip_serializing_if = "Option::is_none")]
        query_facts: Option<Box<ModuleQueryFacts>>,
    },
    HeaderParseFailed {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        query_facts: Option<ModuleQueryFacts>,
    },
    Unsupported,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BatchProjection {
    Diagnostics(DiagnosticsBlock),
    ProofState(ProofStateProjection),
    TypeAt(TypeAtProjection),
    References(ReferencesProjection),
    DeclarationTarget(DeclarationTargetProjection),
    SurroundingDeclaration(SurroundingDeclarationProjection),
}

/// Inspect the current Lean proof context at a declaration proof position.
///
/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read and
/// `ServerError::Lean` for worker infrastructure failures.
pub async fn proof_state(ctx: &ToolContext, req: ProofStateRequest) -> Result<Response<ProofStateResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let selectors = vec![
        LeanWorkerModuleQuerySelector::Diagnostics {
            id: PROOF_STATE_DIAGNOSTICS_ID.to_owned(),
        },
        LeanWorkerModuleQuerySelector::ProofStateInDeclaration {
            id: PROOF_STATE_CONTEXT_ID.to_owned(),
            declaration: req.declaration,
            position: worker_proof_position(req.proof_position),
            // Pretty, notation-aware locals (the worker default); the raw
            // `Expr` form is an expert-only opt-in we do not surface here.
            locals_raw: false,
        },
    ];
    let budgets = proof_agent_budgets();
    // A missing-`.olean` in the declaration's own import closure means the batch
    // could not run; degrade to the shared needs_build verdict instead of
    // letting the raw error propagate as an MCP transport error.
    let run = match crate::diagnosis::classify_missing_olean(
        run_module_query_batch(ctx, hint.clone(), &meta.canonical_root, &req.file, selectors, budgets).await,
    )? {
        crate::diagnosis::CallOutcome::Ready(run) => run,
        crate::diagnosis::CallOutcome::NeedsBuild(err) => {
            // Forward the file's real header imports so the degraded envelope's
            // `freshness.imports` matches the success path (re-read on this rare
            // arm only; empty if the file is now unreadable).
            let imports = read_query_file(&meta.canonical_root, &req.file)
                .map(|input| input.imports)
                .unwrap_or_default();
            return proof_state_needs_build_response(ctx, hint, imports, err);
        }
    };
    let freshness = run.freshness.clone();

    match run.outcome {
        BatchQueryRun::Ready {
            result,
            facts,
            missing_imports,
        } => {
            // `ModuleQueryFacts` is pure operational telemetry; build it only
            // when the agent opted into `full` verbosity.
            let query_facts = ctx.config.verbosity.is_full().then(|| project_query_facts(facts));
            let mut diagnostics = DiagnosticsBlock {
                summary: DiagnosticSummary::default(),
                diagnostics: Vec::new(),
                truncated: false,
            };
            let mut declaration_name = None;
            let mut namespace_name = None;
            let mut goals_before = Vec::new();
            let mut goals_after = Vec::new();
            let mut locals = Vec::new();
            let mut expected_type = None;
            let mut truncated = false;
            let mut unavailable = Vec::new();
            let mut needs_build = Vec::new();
            let mut needs_build_missing: Vec<String> = Vec::new();
            let mut ambiguous: Vec<DeclarationTargetInfo> = Vec::new();
            let mut budget_exceeded = Vec::new();

            for item in result.items {
                match project_batch_item(item, None) {
                    ProjectedBatchItem::Ok { id, result } => match (id.as_str(), result) {
                        (PROOF_STATE_DIAGNOSTICS_ID, BatchProjection::Diagnostics(block)) => {
                            diagnostics = block;
                        }
                        (PROOF_STATE_CONTEXT_ID, BatchProjection::ProofState(value)) => match value {
                            ProofStateProjection::State { info } => {
                                let info = *info;
                                declaration_name = info.declaration_name;
                                namespace_name = Some(info.namespace_name);
                                goals_before = info.goals_before;
                                goals_after = info.goals_after;
                                locals = info.locals;
                                expected_type = info.expected_type;
                                truncated = info.truncated;
                            }
                            ProofStateProjection::Unavailable { message } => {
                                unavailable.push(SelectorMessage { id, message });
                            }
                            // The worker (protocol 8) classifies an incomplete
                            // environment and a genuine collision as typed
                            // verdicts; route each to its honest bucket.
                            ProofStateProjection::NeedsBuild { missing } => {
                                let message = if missing.is_empty() {
                                    "project environment is incomplete".to_owned()
                                } else {
                                    format!("missing: {}", missing.join(", "))
                                };
                                needs_build_missing.extend(missing);
                                needs_build.push(SelectorMessage { id, message });
                            }
                            ProofStateProjection::Ambiguous { candidates } => {
                                ambiguous.extend(candidates);
                            }
                        },
                        _ => {}
                    },
                    ProjectedBatchItem::Unavailable { id, message } => {
                        unavailable.push(SelectorMessage { id, message });
                    }
                    ProjectedBatchItem::BudgetExceeded { id, message } => {
                        budget_exceeded.push(SelectorMessage { id, message });
                    }
                }
            }

            let needs_build_cue = (!needs_build.is_empty()).then(|| needs_build_missing.clone());
            let ambiguous_cue = competing_decls(&ambiguous);
            let response = Response::ok(
                ProofStateResult::Context {
                    diagnostics,
                    declaration_name,
                    namespace_name,
                    goals_before,
                    goals_after,
                    locals,
                    expected_type,
                    truncated,
                    total_truncated: result.total_truncated,
                    unavailable,
                    needs_build,
                    ambiguous,
                    budget_exceeded,
                    query_facts: query_facts.map(Box::new),
                },
                freshness,
            )
            .with_runtime(run.runtime.clone());
            let response = match needs_build_cue {
                Some(missing) => crate::diagnosis::warn_needs_build(
                    response,
                    &crate::diagnosis::IncompleteCause::MissingImports(missing),
                ),
                None => response,
            };
            let response = crate::diagnosis::warn_ambiguous(response, &ambiguous_cue);
            // A recycle mid-batch can leave the goal context empty or degraded,
            // which reads as a clean "proof done"; flag it honestly.
            let response = match crate::diagnosis::execution_taint(&run.runtime) {
                Some(event) => crate::diagnosis::warn_execution_taint(response, event),
                None => response,
            };
            Ok(warn_session_missing_imports(response, &missing_imports))
        }
        BatchQueryRun::HeaderParseFailed { diagnostics, facts } => {
            let block = diagnostics_block(diagnostics);
            Ok(Response::ok(
                ProofStateResult::HeaderParseFailed {
                    summary: block.summary,
                    diagnostics: block.diagnostics,
                    truncated: block.truncated,
                    query_facts: ctx.config.verbosity.is_full().then(|| project_query_facts(facts)),
                },
                freshness,
            )
            .with_runtime(run.runtime))
        }
        BatchQueryRun::Unsupported => {
            Ok(Response::ok(ProofStateResult::Unsupported, freshness).with_runtime(run.runtime))
        }
    }
}

/// Build the degraded proof-state context when the declaration's import
/// closure hit an unbuilt `.olean`: the batch could not run, so report a
/// `needs_build` selector plus the canonical `lake build` warning — the same
/// honest verdict the worker-typed `NeedsBuild` routing produces, rather than a
/// raw transport error. Freshness/runtime come from the non-spawning broker
/// identity path, paid only on this rare arm.
fn proof_state_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    err: ServerError,
) -> Result<Response<ProofStateResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, imports)?;
    let message = err.to_string().lines().next().unwrap_or_default().trim().to_owned();
    let response = Response::ok(needs_build_context(message), base.freshness).with_runtime(base.runtime);
    Ok(crate::diagnosis::warn_needs_build(
        response,
        &crate::diagnosis::IncompleteCause::MissingOlean(err.to_string()),
    ))
}

/// Proof-state context for an unbuilt-dependency degrade: no goals, one
/// `needs_build` selector entry. Pure, for unit testing.
fn needs_build_context(message: String) -> ProofStateResult {
    ProofStateResult::Context {
        diagnostics: DiagnosticsBlock {
            summary: DiagnosticSummary::default(),
            diagnostics: Vec::new(),
            truncated: false,
        },
        declaration_name: None,
        namespace_name: None,
        goals_before: Vec::new(),
        goals_after: Vec::new(),
        locals: Vec::new(),
        expected_type: None,
        truncated: false,
        total_truncated: false,
        unavailable: Vec::new(),
        needs_build: vec![SelectorMessage {
            id: PROOF_STATE_CONTEXT_ID.to_owned(),
            message,
        }],
        ambiguous: Vec::new(),
        budget_exceeded: Vec::new(),
        // No query ran on this degrade path, so there are no facts to report.
        query_facts: None,
    }
}

// --- references --------------------------------------------------------

/// Search the declaration's own file only, or the whole project.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceScope {
    File,
    Project,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindReferencesRequest {
    /// Fully-qualified Lean name to find references to.
    pub name: String,
    /// Search the declaration's file only, or the whole project.
    pub scope: ReferenceScope,
    /// Anchor file whose import context resolves `name`; relative paths resolve
    /// against the project root.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Restrict a `project` scan to these files; relative to the project root.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Maximum references to return.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeaderParseFailedFile {
    pub file: String,
    pub diagnostics: ElabFailure,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MissingImportsFile {
    pub file: String,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FindReferencesResult {
    Ok {
        references: Vec<ReferenceHit>,
        /// The `limit` (or `MAX_REFERENCES`) cap was hit; the returned set is a
        /// stable prefix of the full answer.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        /// Project scope: `.ilean` modules parsed into the index. File scope:
        /// the single anchor file, if it elaborated.
        files_scanned: usize,
        /// Project scope: `.ilean` modules skipped because they were unreadable,
        /// malformed, or an unsupported version. File scope: the anchor file, if
        /// it could not be read.
        files_skipped: usize,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unsupported_files: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        header_parse_failed_files: Vec<HeaderParseFailedFile>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        missing_imports_files: Vec<MissingImportsFile>,
        semantic_based: bool,
    },
    InvalidRequest {
        message: String,
        semantic_based: bool,
    },
}

/// # Errors
///
/// Returns `ServerError::Lean` if an underlying file query fails for an
/// infrastructure reason. Files that cannot be read are counted as skipped
/// files so a bounded project-scope lookup can continue.
pub async fn find_references(ctx: &ToolContext, req: FindReferencesRequest) -> Result<Response<FindReferencesResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    let freshness = base.freshness;
    let runtime = base.runtime;
    let root = meta.canonical_root;
    let limit = req.limit.unwrap_or(MAX_REFERENCES).min(MAX_REFERENCES);

    // The two scopes answer genuinely different questions and so read from
    // different sources (ch.17): `file` scope wants *edit-fresh* results for the
    // file under the cursor, so it elaborates that one file through the worker;
    // `project` scope wants the whole-project answer, which the on-disk `.ilean`
    // reference index already holds — *build-fresh* — and reads in milliseconds
    // with no worker query. The freshness asymmetry is the point, not a bug.
    match req.scope {
        ReferenceScope::File => find_references_in_file(ctx, hint, &root, &req, freshness, runtime, limit).await,
        ReferenceScope::Project => Ok(find_references_in_project(&root, &req, freshness, runtime, limit)),
    }
}

/// File scope: elaborate the single anchor file through the worker so results
/// reflect the file's *current* source (edit-fresh). One file is already bounded
/// by the broker's per-request timeout, so this path carries no wall-clock
/// scan deadline.
async fn find_references_in_file(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    req: &FindReferencesRequest,
    freshness: Freshness,
    mut runtime: RuntimeFacts,
    limit: usize,
) -> Result<Response<FindReferencesResult>> {
    let Some(file) = req.file.as_ref() else {
        return Ok(Response::ok(
            FindReferencesResult::InvalidRequest {
                message: "find_references with scope=file requires `file`".to_owned(),
                semantic_based: true,
            },
            freshness,
        )
        .with_runtime(runtime));
    };
    if !req.files.is_empty() {
        return Ok(Response::ok(
            FindReferencesResult::InvalidRequest {
                message: "find_references with scope=file accepts `file`, not `files`".to_owned(),
                semantic_based: true,
            },
            freshness,
        )
        .with_runtime(runtime));
    }

    let path = resolve_path(root, file);
    let display = display_path(root, &path);
    let query = LeanWorkerModuleQuery::References { name: req.name.clone() };

    let mut hits: Vec<ReferenceHit> = Vec::new();
    let mut unsupported_files: Vec<String> = Vec::new();
    let mut header_parse_failed_files: Vec<HeaderParseFailedFile> = Vec::new();
    let mut missing_imports_files: Vec<MissingImportsFile> = Vec::new();
    let mut truncated = false;
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
    let mut any_freshly_processed = false;
    // An unbuilt transitive dependency surfaces as a missing-`.olean` error; the
    // file is skipped and the call degrades to the `needs_build` verdict (with
    // the `lake build` cue) rather than hard-erroring.
    let mut needs_build: Option<ServerError> = None;

    match run_module_query(ctx, hint, root, &path, query).await {
        Ok(QueryRun {
            outcome:
                ModuleQueryRun::Ready {
                    result: LeanWorkerModuleQueryResult::References(result),
                    freshly_processed,
                    missing_imports,
                },
            runtime: run_runtime,
        }) => {
            runtime = run_runtime;
            files_scanned = files_scanned.saturating_add(1);
            any_freshly_processed |= freshly_processed;
            if !missing_imports.is_empty() {
                missing_imports_files.push(MissingImportsFile {
                    file: display.clone(),
                    missing: missing_imports,
                });
            }
            if result.truncated {
                truncated = true;
            }
            for node in &result.references {
                if hits.len() >= limit {
                    truncated = true;
                    break;
                }
                hits.push(project_reference(&display, node));
            }
        }
        Ok(QueryRun {
            outcome:
                ModuleQueryRun::Ready {
                    freshly_processed,
                    missing_imports,
                    ..
                },
            runtime: run_runtime,
        }) => {
            runtime = run_runtime;
            files_scanned = files_scanned.saturating_add(1);
            any_freshly_processed |= freshly_processed;
            if !missing_imports.is_empty() {
                missing_imports_files.push(MissingImportsFile {
                    file: display,
                    missing: missing_imports,
                });
            }
        }
        Ok(QueryRun {
            outcome: ModuleQueryRun::HeaderParseFailed { diagnostics },
            runtime: run_runtime,
        }) => {
            runtime = run_runtime;
            header_parse_failed_files.push(HeaderParseFailedFile {
                file: display,
                diagnostics,
            });
        }
        Ok(QueryRun {
            outcome: ModuleQueryRun::Unsupported,
            runtime: run_runtime,
        }) => {
            runtime = run_runtime;
            unsupported_files.push(display);
        }
        Err(ServerError::Io(_)) => {
            files_skipped = files_skipped.saturating_add(1);
        }
        Err(err) if crate::diagnosis::missing_olean_failure(&err) => {
            files_skipped = files_skipped.saturating_add(1);
            needs_build = Some(err);
        }
        Err(err) => return Err(err),
    }

    hits.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });

    let result = FindReferencesResult::Ok {
        references: hits,
        truncated,
        files_scanned,
        files_skipped,
        unsupported_files,
        header_parse_failed_files,
        missing_imports_files,
        semantic_based: true,
    };
    let response = attach_query_notes(
        Response::ok(result, freshness).with_runtime(runtime),
        any_freshly_processed,
        &[],
        false,
    );
    let response = match needs_build {
        Some(err) => crate::diagnosis::warn_needs_build(
            response,
            &crate::diagnosis::IncompleteCause::MissingOlean(err.to_string()),
        ),
        None => response,
    };
    Ok(response)
}

/// Project scope: read the on-disk `.ilean` reference index for the whole
/// project (build-fresh) in milliseconds, with no worker query. Returns the
/// complete answer up to `limit`; an unbuilt project degrades to `needs_build`
/// and an index stale relative to current source rides a freshness note.
fn find_references_in_project(
    root: &Path,
    req: &FindReferencesRequest,
    freshness: Freshness,
    runtime: RuntimeFacts,
    limit: usize,
) -> Response<FindReferencesResult> {
    if req.file.is_some() {
        return Response::ok(
            FindReferencesResult::InvalidRequest {
                message: "find_references with scope=project accepts `files`, not `file`".to_owned(),
                semantic_based: true,
            },
            freshness,
        )
        .with_runtime(runtime);
    }

    let index = crate::ilean::references_to(root, &req.name);

    // A wholly-absent build directory is the honest "the project is not built"
    // verdict — degrade to the shared `needs_build` warning, never a silent
    // empty result (which would read as "no references"). A built tree with zero
    // hits for the name is a legitimate empty answer, not a degrade.
    if index.status == crate::ilean::IndexStatus::NotBuilt {
        let response = Response::ok(empty_references_result(0, 0), freshness).with_runtime(runtime);
        return crate::diagnosis::warn_needs_build(
            response,
            &crate::diagnosis::IncompleteCause::MissingImports(Vec::new()),
        );
    }

    // The reader takes no file subset, so a `files`-restricted request reads the
    // one whole-project index (O(ms)) and filters by relative-to-root path.
    let restrict: Option<std::collections::HashSet<String>> = (!req.files.is_empty()).then(|| {
        req.files
            .iter()
            .map(|p| display_path(root, &resolve_path(root, p)))
            .collect()
    });

    let mut hits: Vec<ReferenceHit> = Vec::new();
    for loc in &index.references {
        let display = display_path(root, &loc.file);
        if restrict.as_ref().is_some_and(|set| !set.contains(&display)) {
            continue;
        }
        hits.push(index_reference(&display, loc));
    }

    // Sort then cap so the `limit` truncation is deterministic — a stable
    // prefix, not the arbitrary partial sweep the old per-file deadline returned.
    hits.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    let truncated = hits.len() > limit;
    hits.truncate(limit);

    let result = FindReferencesResult::Ok {
        references: hits,
        truncated,
        files_scanned: index.modules_scanned,
        files_skipped: index.modules_skipped,
        unsupported_files: Vec::new(),
        header_parse_failed_files: Vec::new(),
        missing_imports_files: Vec::new(),
        semantic_based: true,
    };
    let response = Response::ok(result, freshness).with_runtime(runtime);
    if index.stale_sources.is_empty() {
        return response;
    }
    let names = index
        .stale_sources
        .iter()
        .take(3)
        .map(|p| display_path(root, p))
        .collect::<Vec<_>>()
        .join(", ");
    let more = index.stale_sources.len().saturating_sub(3);
    let suffix = if more > 0 {
        format!(", … (+{more} more)")
    } else {
        String::new()
    };
    response
        .warn(format!(
            "reference index is build-fresh, not edit-fresh: {} contributing module(s) have source newer than \
             their .ilean ({names}{suffix}); results reflect the last `lake build`.",
            index.stale_sources.len()
        ))
        .hint("re-run `lake build` to refresh the reference index for edited modules")
}

/// An empty `Ok` reference result, used for the `needs_build` degrade where the
/// query produced no hits but the warning — not the empty list — carries the
/// verdict.
fn empty_references_result(files_scanned: usize, files_skipped: usize) -> FindReferencesResult {
    FindReferencesResult::Ok {
        references: Vec::new(),
        truncated: false,
        files_scanned,
        files_skipped,
        unsupported_files: Vec::new(),
        header_parse_failed_files: Vec::new(),
        missing_imports_files: Vec::new(),
        semantic_based: true,
    }
}

/// Project an index location onto a wire `ReferenceHit`. The index stores
/// 0-based LSP line/column; the worker `References` path emits 1-based line and
/// 1-based column (the shim's `FileMap.toPosition` is 1-based line / 0-based
/// column, then `+1` on column), so `+1` on both line and column makes the two
/// paths' coordinates identical to the caller.
fn index_reference(file: &str, loc: &crate::ilean::ReferenceLocation) -> ReferenceHit {
    ReferenceHit {
        file: file.to_owned(),
        line: loc.start_line.saturating_add(1),
        column: loc.start_column.saturating_add(1),
        end_line: loc.end_line.saturating_add(1),
        end_column: loc.end_column.saturating_add(1),
        kind: match loc.kind {
            crate::ilean::RefKind::Def => "def",
            crate::ilean::RefKind::Ref => "ref",
        },
    }
}

// --- shared plumbing ---------------------------------------------------

struct QueryRun {
    outcome: ModuleQueryRun,
    runtime: RuntimeFacts,
}

enum ModuleQueryRun {
    Ready {
        result: LeanWorkerModuleQueryResult,
        freshly_processed: bool,
        missing_imports: Vec<String>,
    },
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

struct BatchRun {
    outcome: BatchQueryRun,
    runtime: RuntimeFacts,
    freshness: Freshness,
}

enum BatchQueryRun {
    Ready {
        result: lean_rs_worker_parent::LeanWorkerModuleQueryBatchEnvelope,
        facts: LeanWorkerModuleQueryCacheFacts,
        missing_imports: Vec<String>,
    },
    HeaderParseFailed {
        diagnostics: ElabFailure,
        facts: LeanWorkerModuleQueryCacheFacts,
    },
    Unsupported,
}

async fn run_module_query(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    path: &Path,
    query: LeanWorkerModuleQuery,
) -> Result<QueryRun> {
    let input = read_query_file(root, path)?;
    let session_imports = session_imports(header_imports(&input.source));
    let call = ctx
        .broker
        .process_cached_module_query(
            hint,
            input.resolved,
            input.hash,
            session_imports,
            input.imports,
            input.source,
            query,
            LeanWorkerElabOptions::new(),
        )
        .await?;
    Ok(QueryRun {
        outcome: route_query_outcome(call.value, call.freshly_processed),
        runtime: call.runtime,
    })
}

async fn run_module_query_batch(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    path: &Path,
    selectors: Vec<LeanWorkerModuleQuerySelector>,
    budgets: LeanWorkerOutputBudgets,
) -> Result<BatchRun> {
    let input = read_query_file(root, path)?;
    let file_label = input.resolved.to_string_lossy().into_owned();
    let session_imports = session_imports(header_imports(&input.source));
    let call = ctx
        .broker
        .process_cached_module_query_batch(
            hint,
            input.resolved,
            input.hash,
            session_imports,
            input.imports.clone(),
            input.source,
            selectors,
            budgets,
            LeanWorkerElabOptions::new().file_label(&file_label),
        )
        .await?;
    Ok(BatchRun {
        outcome: route_batch_outcome(call.value),
        runtime: call.runtime,
        freshness: call.freshness,
    })
}

fn proof_agent_budgets() -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: PROOF_AGENT_PER_FIELD_BYTES,
        total_bytes: DEFAULT_TOTAL_BYTES,
    }
}

fn expert_query_budgets() -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: EXPERT_QUERY_PER_FIELD_BYTES,
        total_bytes: DEFAULT_TOTAL_BYTES,
    }
}

#[allow(
    dead_code,
    reason = "prompt 38 establishes the shared budget helper for reference-query callers"
)]
fn reference_query_budgets() -> LeanWorkerOutputBudgets {
    expert_query_budgets()
}

fn route_query_outcome(outcome: LeanWorkerModuleQueryOutcome, freshly_processed: bool) -> ModuleQueryRun {
    match outcome {
        LeanWorkerModuleQueryOutcome::Ok { result, .. } => ModuleQueryRun::Ready {
            result,
            freshly_processed,
            missing_imports: Vec::new(),
        },
        LeanWorkerModuleQueryOutcome::MissingImports { result, missing, .. } => ModuleQueryRun::Ready {
            result,
            freshly_processed,
            missing_imports: missing,
        },
        LeanWorkerModuleQueryOutcome::HeaderParseFailed { diagnostics } => ModuleQueryRun::HeaderParseFailed {
            diagnostics: project_failure(&diagnostics),
        },
        LeanWorkerModuleQueryOutcome::Unsupported => ModuleQueryRun::Unsupported,
        _ => ModuleQueryRun::Unsupported,
    }
}

fn route_batch_outcome(outcome: LeanWorkerModuleQueryBatchOutcome) -> BatchQueryRun {
    match outcome {
        LeanWorkerModuleQueryBatchOutcome::Ok { result, facts, .. } => BatchQueryRun::Ready {
            result,
            facts,
            missing_imports: Vec::new(),
        },
        LeanWorkerModuleQueryBatchOutcome::MissingImports {
            result, missing, facts, ..
        } => BatchQueryRun::Ready {
            result,
            facts,
            missing_imports: missing,
        },
        LeanWorkerModuleQueryBatchOutcome::HeaderParseFailed { diagnostics, facts } => {
            BatchQueryRun::HeaderParseFailed {
                diagnostics: project_failure(&diagnostics),
                facts,
            }
        }
        LeanWorkerModuleQueryBatchOutcome::Unsupported => BatchQueryRun::Unsupported,
        _ => BatchQueryRun::Unsupported,
    }
}

enum ProjectedBatchItem {
    Ok { id: String, result: BatchProjection },
    Unavailable { id: String, message: String },
    BudgetExceeded { id: String, message: String },
}

fn project_batch_item(item: LeanWorkerModuleQueryBatchItem, file: Option<&str>) -> ProjectedBatchItem {
    match item {
        LeanWorkerModuleQueryBatchItem::Ok { id, result } => ProjectedBatchItem::Ok {
            id,
            result: project_batch_result(*result, file),
        },
        LeanWorkerModuleQueryBatchItem::Unavailable { id, message } => ProjectedBatchItem::Unavailable { id, message },
        LeanWorkerModuleQueryBatchItem::BudgetExceeded { id, message } => {
            ProjectedBatchItem::BudgetExceeded { id, message }
        }
        _ => ProjectedBatchItem::Unavailable {
            id: "<unknown>".to_owned(),
            message: "worker returned an unknown selector item".to_owned(),
        },
    }
}

fn project_batch_result(result: LeanWorkerModuleQueryBatchResult, file: Option<&str>) -> BatchProjection {
    match result {
        LeanWorkerModuleQueryBatchResult::Diagnostics(failure) => {
            BatchProjection::Diagnostics(diagnostics_block(project_failure(&failure)))
        }
        LeanWorkerModuleQueryBatchResult::ProofState(result) => {
            BatchProjection::ProofState(project_proof_state_result(result))
        }
        LeanWorkerModuleQueryBatchResult::TypeAt(result) => BatchProjection::TypeAt(project_type_at_result(result)),
        LeanWorkerModuleQueryBatchResult::References(result) => {
            let display = file.unwrap_or("");
            BatchProjection::References(ReferencesProjection {
                references: result
                    .references
                    .iter()
                    .map(|node| project_reference(display, node))
                    .collect(),
                truncated: result.truncated,
            })
        }
        LeanWorkerModuleQueryBatchResult::DeclarationTarget(result) => {
            BatchProjection::DeclarationTarget(project_declaration_target_result(result))
        }
        LeanWorkerModuleQueryBatchResult::SurroundingDeclaration(result) => {
            BatchProjection::SurroundingDeclaration(project_surrounding_declaration_result(result))
        }
        LeanWorkerModuleQueryBatchResult::DeclarationOutline(_) => BatchProjection::Diagnostics(DiagnosticsBlock {
            summary: DiagnosticSummary::default(),
            diagnostics: Vec::new(),
            truncated: false,
        }),
        _ => BatchProjection::Diagnostics(DiagnosticsBlock {
            summary: DiagnosticSummary::default(),
            diagnostics: Vec::new(),
            truncated: false,
        }),
    }
}

fn project_proof_state_result(result: LeanWorkerProofStateResult) -> ProofStateProjection {
    match result {
        LeanWorkerProofStateResult::State { info } => ProofStateProjection::State {
            info: Box::new(project_proof_state_info(*info)),
        },
        LeanWorkerProofStateResult::Unavailable { message } => ProofStateProjection::Unavailable { message },
        LeanWorkerProofStateResult::Ambiguous { candidates } => ProofStateProjection::Ambiguous {
            candidates: candidates.into_iter().map(project_declaration_target_info).collect(),
        },
        LeanWorkerProofStateResult::NeedsBuild { missing } => ProofStateProjection::NeedsBuild { missing },
        _ => ProofStateProjection::Unavailable {
            message: "worker returned an unknown proof-state result".to_owned(),
        },
    }
}

fn project_proof_state_info(info: LeanWorkerProofStateInfo) -> ProofStateContext {
    ProofStateContext {
        declaration_name: info.declaration_name,
        namespace_name: info.namespace_name,
        goals_before: info.goals_before,
        goals_after: info.goals_after,
        locals: info.locals.into_iter().map(project_local_info).collect(),
        expected_type: info.expected_type.map(rendered_text),
        truncated: info.truncated,
    }
}

fn project_local_info(info: LeanWorkerLocalInfo) -> LocalInfo {
    LocalInfo {
        name: info.name,
        binder_info: info.binder_info,
        type_str: rendered_text(info.type_str),
        value: info.value.map(rendered_text),
    }
}

fn project_type_at_result(result: LeanWorkerTypeAtResult) -> TypeAtProjection {
    match result {
        LeanWorkerTypeAtResult::Term {
            span,
            expr,
            type_str,
            expected_type,
        } => TypeAtProjection::Term {
            expr: rendered_text(expr),
            type_str: rendered_text(type_str),
            expected_type: expected_type.map(rendered_text),
            span: span_of_module(span),
        },
        LeanWorkerTypeAtResult::NoTerm => TypeAtProjection::NoTerm,
        _ => TypeAtProjection::NoTerm,
    }
}

fn project_declaration_target_result(result: LeanWorkerDeclarationTargetResult) -> DeclarationTargetProjection {
    match result {
        LeanWorkerDeclarationTargetResult::Target { info } => DeclarationTargetProjection::Target {
            info: project_declaration_target_info(info),
        },
        LeanWorkerDeclarationTargetResult::NotFound => DeclarationTargetProjection::NotFound,
        LeanWorkerDeclarationTargetResult::Ambiguous { candidates } => DeclarationTargetProjection::Ambiguous {
            candidates: candidates.into_iter().map(project_declaration_target_info).collect(),
        },
        _ => DeclarationTargetProjection::NotFound,
    }
}

fn project_surrounding_declaration_result(
    result: LeanWorkerSurroundingDeclarationResult,
) -> SurroundingDeclarationProjection {
    match result {
        LeanWorkerSurroundingDeclarationResult::Declaration { info } => SurroundingDeclarationProjection::Declaration {
            info: project_declaration_target_info(info),
        },
        LeanWorkerSurroundingDeclarationResult::None => SurroundingDeclarationProjection::None,
        _ => SurroundingDeclarationProjection::None,
    }
}

/// Map projected ambiguity candidates to the diagnosis renderer's shape. The
/// fully-qualified `declaration_name` plus `namespace_name` disambiguator is
/// what an agent needs to pick the intended declaration.
fn competing_decls(candidates: &[DeclarationTargetInfo]) -> Vec<crate::diagnosis::CompetingDecl> {
    candidates
        .iter()
        .map(|info| crate::diagnosis::CompetingDecl {
            name: info.declaration_name.clone(),
            namespace: (!info.namespace_name.is_empty()).then(|| info.namespace_name.clone()),
        })
        .collect()
}

fn project_declaration_target_info(info: LeanWorkerDeclarationTargetInfo) -> DeclarationTargetInfo {
    DeclarationTargetInfo {
        short_name: info.short_name,
        declaration_name: info.declaration_name,
        namespace_name: info.namespace_name,
        declaration_kind: info.declaration_kind,
        declaration_span: span_of_module(info.declaration_span),
        name_span: span_of_module(info.name_span),
        body_span: span_of_module(info.body_span),
    }
}

fn diagnostics_block(failure: ElabFailure) -> DiagnosticsBlock {
    let ElabFailure { diagnostics, truncated } = failure;
    let diagnostics = sort_diagnostics(diagnostics);
    DiagnosticsBlock {
        summary: DiagnosticSummary::from_diagnostics(&diagnostics),
        diagnostics,
        truncated,
    }
}

fn sort_diagnostics(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort_by_key(|d| d.position.as_ref().map_or((u32::MAX, u32::MAX), |p| (p.line, p.column)));
    diagnostics
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

fn span_of_module(span: LeanWorkerModuleSourceSpan) -> SourceSpan {
    SourceSpan {
        start_line: span.start_line,
        start_column: span.start_column,
        end_line: span.end_line,
        end_column: span.end_column,
    }
}

fn rendered_text(info: LeanWorkerRenderedInfo) -> RenderedText {
    RenderedText {
        value: info.value,
        truncated: info.truncated,
    }
}

fn project_query_facts(facts: LeanWorkerModuleQueryCacheFacts) -> ModuleQueryFacts {
    ModuleQueryFacts {
        cache_status: cache_status_label(facts.cache_status),
        output_bytes: facts.output_bytes,
        cache_entry_count: facts.cache_entry_count,
        cache_approx_bytes: facts.cache_approx_bytes,
        timings: project_query_timings(facts.timings),
    }
}

fn cache_status_label(status: LeanWorkerModuleCacheStatus) -> &'static str {
    match status {
        LeanWorkerModuleCacheStatus::Hit => "hit",
        LeanWorkerModuleCacheStatus::Miss => "miss",
        LeanWorkerModuleCacheStatus::Rebuilt => "rebuilt",
        LeanWorkerModuleCacheStatus::Evicted => "evicted",
        _ => "unknown",
    }
}

fn project_query_timings(timings: LeanWorkerModuleQueryTimings) -> ModuleQueryTimings {
    ModuleQueryTimings {
        header_import_micros: timings.header_import_micros,
        elaboration_micros: timings.elaboration_micros,
        projection_micros: timings.projection_micros,
        rendering_micros: timings.rendering_micros,
    }
}

fn project_reference(file: &str, node: &LeanWorkerNameRef) -> ReferenceHit {
    ReferenceHit {
        file: file.to_owned(),
        line: node.start_line,
        column: node.start_column,
        end_line: node.end_line,
        end_column: node.end_column,
        kind: if node.is_binder { "def" } else { "ref" },
    }
}

fn attach_query_notes<T>(
    mut response: Response<T>,
    freshly_processed: bool,
    missing_imports: &[String],
    batch: bool,
) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    if freshly_processed {
        let kind = if batch { "module query batch" } else { "module query" };
        response.next_actions.push(format!(
            "{kind} result cached; repeating the same query against the same file contents reuses it"
        ));
    }
    warn_session_missing_imports(response, missing_imports)
}

/// Warn when the file header imports modules the opened worker session lacks —
/// the agent's queries see a partial environment until those imports build.
fn warn_session_missing_imports<T>(mut response: Response<T>, missing_imports: &[String]) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    if !missing_imports.is_empty() {
        response.warnings.push(format!(
            "file header referenced imports not present in the opened session: {}",
            missing_imports.join(", ")
        ));
    }
    response
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "unit tests should fail directly when literal JSON fixtures stop matching the schema"
)]
mod tests {
    use lean_rs_worker_parent::{
        LeanWorkerElabFailure, LeanWorkerModuleCacheStatus, LeanWorkerModuleQueryBatchEnvelope,
        LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryCacheFacts, LeanWorkerModuleQueryTimings,
    };

    use super::{
        BatchQueryRun, DiagnosticSummary, DiagnosticsBlock, LeanWorkerProofPositionSelector, ProofPositionSelector,
        ProofStateRequest, ProofStateResult, RenderedText, cache_status_label, expert_query_budgets,
        needs_build_context, project_query_facts, proof_agent_budgets, reference_query_budgets, route_batch_outcome,
        worker_proof_position,
    };
    use crate::tools::source_input::{header_imports, read_query_file};

    fn worker_facts(status: LeanWorkerModuleCacheStatus) -> LeanWorkerModuleQueryCacheFacts {
        LeanWorkerModuleQueryCacheFacts {
            cache_status: status,
            timings: LeanWorkerModuleQueryTimings {
                header_import_micros: 1,
                elaboration_micros: 2,
                projection_micros: 3,
                rendering_micros: 4,
            },
            output_bytes: 123,
            cache_entry_count: Some(2),
            cache_approx_bytes: Some(4096),
            resource: None,
        }
    }

    #[test]
    fn header_imports_handles_public_meta_and_all() {
        let source = "\
module

public import Foo.Bar
meta import Baz.Qux
import all Project.Internal
import Init -- comment
";
        assert_eq!(
            header_imports(source),
            vec![
                "Foo.Bar".to_owned(),
                "Baz.Qux".to_owned(),
                "Project.Internal".to_owned(),
                "Init".to_owned(),
            ]
        );
    }

    #[test]
    fn proof_state_request_round_trips() {
        let request: ProofStateRequest =
            serde_json::from_str(r#"{"file":"Foo/Bar.lean","declaration":"Foo.Bar.t"}"#).unwrap();
        assert_eq!(request.declaration, "Foo.Bar.t");
        assert!(matches!(request.proof_position, ProofPositionSelector::Default));
    }

    #[test]
    fn default_position_maps_to_pristine_entry() {
        // The default targets the pristine entry goal so `proof_state` and
        // `try_proof_step` agree on where the default proof starts; the old
        // first-tactic-state default stays reachable as `index: 0`.
        assert!(matches!(
            worker_proof_position(ProofPositionSelector::Default),
            LeanWorkerProofPositionSelector::Entry
        ));
        assert!(matches!(
            worker_proof_position(ProofPositionSelector::Index { index: 0 }),
            LeanWorkerProofPositionSelector::Index { index: 0 }
        ));
    }

    #[test]
    fn module_query_budget_helpers_set_host_policy() {
        let proof_agent = proof_agent_budgets();
        assert_eq!(proof_agent.per_field_bytes, 4 * 1024);
        assert_eq!(proof_agent.total_bytes, 64 * 1024);

        let expert = expert_query_budgets();
        assert_eq!(expert.per_field_bytes, 8 * 1024);
        assert_eq!(expert.total_bytes, 64 * 1024);

        let references = reference_query_budgets();
        assert_eq!(references, expert);
    }

    #[test]
    fn needs_build_context_reports_a_needs_build_selector_not_unavailable() {
        let result = needs_build_context("object file 'Dep.olean' ... does not exist".to_owned());
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["status"], "context");
        // The blocking condition lands in `needs_build` (with the `lake build`
        // envelope warning), never silently in `unavailable` or as goals.
        assert_eq!(value["needs_build"][0]["id"], "proof_state");
        assert!(value["needs_build"][0]["message"].as_str().unwrap().contains(".olean"));
        assert!(value.get("unavailable").is_none());
        assert!(value.get("goals_before").is_none());
    }

    #[test]
    fn proof_state_context_serializes_flattened_workflow_fields() {
        let result = ProofStateResult::Context {
            diagnostics: DiagnosticsBlock {
                summary: DiagnosticSummary::default(),
                diagnostics: Vec::new(),
                truncated: false,
            },
            declaration_name: Some("Demo.proof".to_owned()),
            namespace_name: Some("Demo".to_owned()),
            goals_before: vec!["⊢ True".to_owned()],
            goals_after: Vec::new(),
            locals: Vec::new(),
            expected_type: Some(RenderedText {
                value: "True".to_owned(),
                truncated: false,
            }),
            truncated: false,
            total_truncated: false,
            unavailable: Vec::new(),
            needs_build: Vec::new(),
            ambiguous: Vec::new(),
            budget_exceeded: Vec::new(),
            query_facts: Some(Box::new(project_query_facts(worker_facts(
                LeanWorkerModuleCacheStatus::Miss,
            )))),
        };

        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["status"], "context");
        assert_eq!(value["declaration_name"], "Demo.proof");
        assert_eq!(value["namespace_name"], "Demo");
        assert_eq!(value["goals_before"][0], "⊢ True");
        assert_eq!(value["expected_type"]["value"], "True");
        assert!(value.get("span").is_none());
        assert!(value.get("safe_edit").is_none());
        assert!(value.get("proof_state").is_none());
        assert!(value.get("declaration_target").is_none());
    }

    #[test]
    fn query_facts_project_cache_status_and_timings() {
        let facts = project_query_facts(worker_facts(LeanWorkerModuleCacheStatus::Hit));

        assert_eq!(facts.cache_status, "hit");
        assert_eq!(facts.output_bytes, 123);
        assert_eq!(facts.cache_entry_count, Some(2));
        assert_eq!(facts.cache_approx_bytes, Some(4096));
        assert_eq!(facts.timings.header_import_micros, 1);
        assert_eq!(facts.timings.elaboration_micros, 2);
        assert_eq!(facts.timings.projection_micros, 3);
        assert_eq!(facts.timings.rendering_micros, 4);
    }

    #[test]
    fn cache_status_labels_match_wire_strings() {
        assert_eq!(cache_status_label(LeanWorkerModuleCacheStatus::Hit), "hit");
        assert_eq!(cache_status_label(LeanWorkerModuleCacheStatus::Miss), "miss");
        assert_eq!(cache_status_label(LeanWorkerModuleCacheStatus::Rebuilt), "rebuilt");
        assert_eq!(cache_status_label(LeanWorkerModuleCacheStatus::Evicted), "evicted");
    }

    #[test]
    fn read_query_file_uses_canonical_path_for_identity() {
        let dir = tempfile::tempdir().unwrap();
        let module_dir = dir.path().join("Demo");
        std::fs::create_dir(&module_dir).unwrap();
        std::fs::write(module_dir.join("Basic.lean"), "import Init\n#check Nat\n").unwrap();

        let input = read_query_file(dir.path(), std::path::Path::new("Demo/../Demo/Basic.lean")).unwrap();

        assert_eq!(input.resolved, module_dir.join("Basic.lean").canonicalize().unwrap());
        assert_eq!(input.imports, vec!["Init".to_owned()]);
        assert_eq!(input.source, "import Init\n#check Nat\n");
    }

    #[test]
    fn route_batch_outcome_preserves_worker_facts() {
        let result = LeanWorkerModuleQueryBatchEnvelope {
            items: Vec::new(),
            total_truncated: false,
        };
        let outcome = route_batch_outcome(LeanWorkerModuleQueryBatchOutcome::Ok {
            result,
            imports: Vec::new(),
            facts: worker_facts(LeanWorkerModuleCacheStatus::Hit),
        });

        let BatchQueryRun::Ready { facts, .. } = outcome else {
            panic!("expected ready batch outcome");
        };
        assert_eq!(cache_status_label(facts.cache_status), "hit");

        let outcome = route_batch_outcome(LeanWorkerModuleQueryBatchOutcome::MissingImports {
            result: LeanWorkerModuleQueryBatchEnvelope {
                items: Vec::new(),
                total_truncated: false,
            },
            imports: Vec::new(),
            missing: vec!["Missing.Mod".to_owned()],
            facts: worker_facts(LeanWorkerModuleCacheStatus::Rebuilt),
        });
        let BatchQueryRun::Ready {
            facts, missing_imports, ..
        } = outcome
        else {
            panic!("expected ready batch outcome with missing imports");
        };
        assert_eq!(cache_status_label(facts.cache_status), "rebuilt");
        assert_eq!(missing_imports, vec!["Missing.Mod".to_owned()]);

        let outcome = route_batch_outcome(LeanWorkerModuleQueryBatchOutcome::HeaderParseFailed {
            diagnostics: LeanWorkerElabFailure {
                diagnostics: Vec::new(),
                truncated: false,
            },
            facts: worker_facts(LeanWorkerModuleCacheStatus::Evicted),
        });
        let BatchQueryRun::HeaderParseFailed { facts, .. } = outcome else {
            panic!("expected header-parse failure");
        };
        assert_eq!(cache_status_label(facts.cache_status), "evicted");
    }
}
