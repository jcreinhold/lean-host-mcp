//! Bounded module-query tools: `proof_state`, `lean_query`, and reference
//! queries.
//!
//! The public proof-agent path uses one batched worker call per file probe.
//! `proof_state` presents a curated proof workflow context; `lean_query`
//! keeps the underlying selector batch available for expert callers.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_worker_parent::{
    LeanWorkerDeclarationTargetInfo, LeanWorkerDeclarationTargetResult, LeanWorkerElabOptions, LeanWorkerLocalInfo,
    LeanWorkerModuleCacheStatus, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchItem,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryBatchResult, LeanWorkerModuleQueryCacheFacts,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQueryResult, LeanWorkerModuleQuerySelector,
    LeanWorkerModuleQueryTimings, LeanWorkerModuleSourceSpan, LeanWorkerNameRef, LeanWorkerOutputBudgets,
    LeanWorkerProofStateInfo, LeanWorkerProofStateResult, LeanWorkerRenderedInfo,
    LeanWorkerSurroundingDeclarationResult, LeanWorkerTypeAtResult,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::broker::ProjectHint;
use crate::cache::{ModuleQueryKey, hash_bytes};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::project::LeanProject;
use crate::projections::{Diagnostic, ElabFailure, Severity, map_worker_err, project_failure};
use crate::tools::{ToolContext, is_ignored_dir, session_imports};

/// Hard cap on project-wide reference aggregation. File-local reference
/// queries are also bounded by the upstream projection.
const MAX_REFERENCES: usize = 1000;

const PROOF_STATE_DIAGNOSTICS_ID: &str = "diagnostics";
const PROOF_STATE_CONTEXT_ID: &str = "proof_state";
const PROOF_STATE_TARGET_ID: &str = "declaration_target";
const PROOF_STATE_SURROUNDING_ID: &str = "surrounding_declaration";
const PROOF_STATE_TERM_ID: &str = "term";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_edit: Option<DeclarationTargetInfo>,
    pub span: SourceSpan,
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
    /// 1-indexed line.
    pub line: u32,
    /// 1-indexed column.
    pub column: u32,
    /// Optional explicit project root for this call.
    #[serde(default)]
    pub project: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        span: Option<SourceSpan>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        goals_before: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        goals_after: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        locals: Vec<LocalInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_type: Option<RenderedText>,
        #[serde(skip_serializing_if = "Option::is_none")]
        safe_edit: Option<Box<DeclarationTargetInfo>>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        target_declaration: Option<Box<DeclarationTargetProjection>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        surrounding_declaration: Option<Box<SurroundingDeclarationProjection>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        term: Option<Box<TypeAtProjection>>,
        total_truncated: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unavailable: Vec<SelectorMessage>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        budget_exceeded: Vec<SelectorMessage>,
        query_facts: Box<ModuleQueryFacts>,
    },
    HeaderParseFailed {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
        query_facts: ModuleQueryFacts,
    },
    Unsupported,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LeanQueryRequest {
    /// Path to a `.lean` file. Resolved against the resolved project root
    /// if relative.
    pub file: PathBuf,
    /// Bounded semantic projections to run against the file in one
    /// elaboration.
    pub selectors: Vec<LeanQuerySelector>,
    /// Optional explicit project root for this call.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "selector", rename_all = "snake_case")]
pub enum LeanQuerySelector {
    Diagnostics {
        id: String,
    },
    ProofState {
        id: String,
        line: u32,
        column: u32,
    },
    TypeAt {
        id: String,
        line: u32,
        column: u32,
    },
    References {
        id: String,
        name: String,
    },
    DeclarationTarget {
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        line: Option<u32>,
        #[serde(default)]
        column: Option<u32>,
    },
    SurroundingDeclaration {
        id: String,
        line: u32,
        column: u32,
    },
}

impl LeanQuerySelector {
    fn id(&self) -> &str {
        match self {
            Self::Diagnostics { id }
            | Self::ProofState { id, .. }
            | Self::TypeAt { id, .. }
            | Self::References { id, .. }
            | Self::DeclarationTarget { id, .. }
            | Self::SurroundingDeclaration { id, .. } => id,
        }
    }

    fn into_worker(self) -> LeanWorkerModuleQuerySelector {
        match self {
            Self::Diagnostics { id } => LeanWorkerModuleQuerySelector::Diagnostics { id },
            Self::ProofState { id, line, column } => LeanWorkerModuleQuerySelector::ProofState { id, line, column },
            Self::TypeAt { id, line, column } => LeanWorkerModuleQuerySelector::TypeAt { id, line, column },
            Self::References { id, name } => LeanWorkerModuleQuerySelector::References { id, name },
            Self::DeclarationTarget { id, name, line, column } => {
                LeanWorkerModuleQuerySelector::DeclarationTarget { id, name, line, column }
            }
            Self::SurroundingDeclaration { id, line, column } => {
                LeanWorkerModuleQuerySelector::SurroundingDeclaration { id, line, column }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeanQueryProjection {
    Diagnostics(DiagnosticsBlock),
    ProofState(ProofStateProjection),
    TypeAt(TypeAtProjection),
    References(ReferencesProjection),
    DeclarationTarget(DeclarationTargetProjection),
    SurroundingDeclaration(SurroundingDeclarationProjection),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LeanQueryItem {
    Ok { result: LeanQueryProjection },
    Unavailable { message: String },
    BudgetExceeded { message: String },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LeanQueryResult {
    Results {
        items: BTreeMap<String, LeanQueryItem>,
        total_truncated: bool,
        query_facts: ModuleQueryFacts,
    },
    HeaderParseFailed {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
        query_facts: ModuleQueryFacts,
    },
    InvalidSelectors {
        message: String,
    },
    Unsupported,
}

/// Inspect the current Lean proof context at a cursor position.
///
/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read and
/// `ServerError::Lean` for worker infrastructure failures.
pub async fn proof_state(ctx: &ToolContext, req: ProofStateRequest) -> Result<Response<ProofStateResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let selectors = vec![
                LeanWorkerModuleQuerySelector::Diagnostics {
                    id: PROOF_STATE_DIAGNOSTICS_ID.to_owned(),
                },
                LeanWorkerModuleQuerySelector::ProofState {
                    id: PROOF_STATE_CONTEXT_ID.to_owned(),
                    line: req.line,
                    column: req.column,
                },
                LeanWorkerModuleQuerySelector::DeclarationTarget {
                    id: PROOF_STATE_TARGET_ID.to_owned(),
                    name: None,
                    line: Some(req.line),
                    column: Some(req.column),
                },
                LeanWorkerModuleQuerySelector::SurroundingDeclaration {
                    id: PROOF_STATE_SURROUNDING_ID.to_owned(),
                    line: req.line,
                    column: req.column,
                },
                LeanWorkerModuleQuerySelector::TypeAt {
                    id: PROOF_STATE_TERM_ID.to_owned(),
                    line: req.line,
                    column: req.column,
                },
            ];
            let budgets = proof_agent_budgets();
            let run = run_module_query_batch(&project, &req.file, selectors, budgets).await?;
            let freshness = project.freshness(&run.imports);

            match run.outcome {
                BatchQueryRun::Ready {
                    result,
                    facts,
                    missing_imports,
                } => {
                    let query_facts = project_query_facts(facts);
                    let mut diagnostics = DiagnosticsBlock {
                        summary: DiagnosticSummary::default(),
                        diagnostics: Vec::new(),
                        truncated: false,
                    };
                    let mut declaration_name = None;
                    let mut namespace_name = None;
                    let mut span = None;
                    let mut goals_before = Vec::new();
                    let mut goals_after = Vec::new();
                    let mut locals = Vec::new();
                    let mut expected_type = None;
                    let mut safe_edit = None;
                    let mut truncated = false;
                    let mut term = None;
                    let mut target_declaration = None;
                    let mut surrounding_declaration = None;
                    let mut unavailable = Vec::new();
                    let mut budget_exceeded = Vec::new();

                    for item in result.items {
                        match project_batch_item(item, None) {
                            ProjectedBatchItem::Ok { id, result } => match (id.as_str(), result) {
                                (PROOF_STATE_DIAGNOSTICS_ID, LeanQueryProjection::Diagnostics(block)) => {
                                    diagnostics = block;
                                }
                                (PROOF_STATE_CONTEXT_ID, LeanQueryProjection::ProofState(value)) => match value {
                                    ProofStateProjection::State { info } => {
                                        let info = *info;
                                        declaration_name = info.declaration_name;
                                        namespace_name = Some(info.namespace_name);
                                        span = Some(info.span);
                                        goals_before = info.goals_before;
                                        goals_after = info.goals_after;
                                        locals = info.locals;
                                        expected_type = info.expected_type;
                                        safe_edit = info.safe_edit.map(Box::new);
                                        truncated = info.truncated;
                                    }
                                    ProofStateProjection::Unavailable { message } => {
                                        unavailable.push(SelectorMessage { id, message });
                                    }
                                },
                                (PROOF_STATE_TARGET_ID, LeanQueryProjection::DeclarationTarget(value)) => {
                                    target_declaration = Some(Box::new(value));
                                }
                                (PROOF_STATE_SURROUNDING_ID, LeanQueryProjection::SurroundingDeclaration(value)) => {
                                    surrounding_declaration = Some(Box::new(value));
                                }
                                (PROOF_STATE_TERM_ID, LeanQueryProjection::TypeAt(value)) => {
                                    term = Some(Box::new(value));
                                }
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

                    let response = Response::ok(
                        ProofStateResult::Context {
                            diagnostics,
                            declaration_name,
                            namespace_name,
                            span,
                            goals_before,
                            goals_after,
                            locals,
                            expected_type,
                            safe_edit,
                            truncated,
                            target_declaration,
                            surrounding_declaration,
                            term,
                            total_truncated: result.total_truncated,
                            unavailable,
                            budget_exceeded,
                            query_facts: Box::new(query_facts.clone()),
                        },
                        freshness,
                    );
                    Ok(attach_batch_query_notes(response, &query_facts, &missing_imports))
                }
                BatchQueryRun::HeaderParseFailed { diagnostics, facts } => {
                    let block = diagnostics_block(diagnostics);
                    Ok(Response::ok(
                        ProofStateResult::HeaderParseFailed {
                            summary: block.summary,
                            diagnostics: block.diagnostics,
                            truncated: block.truncated,
                            query_facts: project_query_facts(facts),
                        },
                        freshness,
                    ))
                }
                BatchQueryRun::Unsupported => Ok(Response::ok(ProofStateResult::Unsupported, freshness)),
            }
        })
        .await
}

/// Run a bounded batch of Lean semantic projections against one file.
///
/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read and
/// `ServerError::Lean` for worker infrastructure failures.
pub async fn lean_query(ctx: &ToolContext, req: LeanQueryRequest) -> Result<Response<LeanQueryResult>> {
    let invalid_selectors = validate_lean_query_selectors(&req.selectors);
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            if let Some(message) = invalid_selectors {
                return Ok(Response::ok(
                    LeanQueryResult::InvalidSelectors { message },
                    project.freshness(&[]),
                ));
            }

            let selectors = req
                .selectors
                .into_iter()
                .map(LeanQuerySelector::into_worker)
                .collect::<Vec<_>>();
            let budgets = expert_query_budgets();
            let run = run_module_query_batch(&project, &req.file, selectors, budgets).await?;
            let freshness = project.freshness(&run.imports);

            match run.outcome {
                BatchQueryRun::Ready {
                    result,
                    facts,
                    missing_imports,
                } => {
                    let query_facts = project_query_facts(facts);
                    let root = project.canonical_root().to_path_buf();
                    let resolved = resolve_path(&root, &req.file);
                    let display = display_path(&root, &resolved);
                    let mut items = BTreeMap::new();
                    for item in result.items {
                        let projected = project_batch_item(item, Some(&display));
                        let (id, value) = projected.into_item();
                        items.insert(id, value);
                    }
                    let response = Response::ok(
                        LeanQueryResult::Results {
                            items,
                            total_truncated: result.total_truncated,
                            query_facts: query_facts.clone(),
                        },
                        freshness,
                    );
                    Ok(attach_batch_query_notes(response, &query_facts, &missing_imports))
                }
                BatchQueryRun::HeaderParseFailed { diagnostics, facts } => {
                    let block = diagnostics_block(diagnostics);
                    Ok(Response::ok(
                        LeanQueryResult::HeaderParseFailed {
                            summary: block.summary,
                            diagnostics: block.diagnostics,
                            truncated: block.truncated,
                            query_facts: project_query_facts(facts),
                        },
                        freshness,
                    ))
                }
                BatchQueryRun::Unsupported => Ok(Response::ok(LeanQueryResult::Unsupported, freshness)),
            }
        })
        .await
}

// --- references --------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceScope {
    File,
    Project,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindReferencesRequest {
    pub name: String,
    pub scope: ReferenceScope,
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub limit: Option<usize>,
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
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        files_scanned: usize,
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
/// infrastructure reason. Files that cannot be read are skipped silently
/// (same policy as [`crate::tools::scan::source_search`]).
pub async fn find_references(ctx: &ToolContext, req: FindReferencesRequest) -> Result<Response<FindReferencesResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = project.freshness(&[]);
            let root = project.canonical_root().to_path_buf();
            let files = match req.scope {
                ReferenceScope::File => {
                    let Some(file) = req.file.as_ref() else {
                        return Ok(Response::ok(
                            FindReferencesResult::InvalidRequest {
                                message: "find_references with scope=file requires `file`".to_owned(),
                                semantic_based: true,
                            },
                            freshness,
                        ));
                    };
                    if !req.files.is_empty() {
                        return Ok(Response::ok(
                            FindReferencesResult::InvalidRequest {
                                message: "find_references with scope=file accepts `file`, not `files`".to_owned(),
                                semantic_based: true,
                            },
                            freshness,
                        ));
                    }
                    vec![resolve_path(&root, file)]
                }
                ReferenceScope::Project => {
                    if req.file.is_some() {
                        return Ok(Response::ok(
                            FindReferencesResult::InvalidRequest {
                                message: "find_references with scope=project accepts `files`, not `file`".to_owned(),
                                semantic_based: true,
                            },
                            freshness,
                        ));
                    }
                    if req.files.is_empty() {
                        enumerate_lean_files(&root)
                    } else {
                        req.files.iter().map(|p| resolve_path(&root, p)).collect()
                    }
                }
            };
            let limit = req.limit.unwrap_or(MAX_REFERENCES).min(MAX_REFERENCES);

            let mut hits: Vec<ReferenceHit> = Vec::new();
            let mut unsupported_files: Vec<String> = Vec::new();
            let mut header_parse_failed_files: Vec<HeaderParseFailedFile> = Vec::new();
            let mut missing_imports_files: Vec<MissingImportsFile> = Vec::new();
            let mut truncated = false;
            let mut files_scanned = 0usize;
            let mut files_skipped = 0usize;
            let mut any_freshly_processed = false;

            'outer: for path in files {
                let display = display_path(&root, &path);
                let query = LeanWorkerModuleQuery::References { name: req.name.clone() };
                match run_module_query(&project, &path, query).await {
                    Ok(QueryRun {
                        outcome:
                            ModuleQueryRun::Ready {
                                result: LeanWorkerModuleQueryResult::References(result),
                                freshly_processed,
                                missing_imports,
                            },
                        ..
                    }) => {
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
                                break 'outer;
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
                        ..
                    }) => {
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
                        ..
                    }) => {
                        header_parse_failed_files.push(HeaderParseFailedFile {
                            file: display,
                            diagnostics,
                        });
                    }
                    Ok(QueryRun {
                        outcome: ModuleQueryRun::Unsupported,
                        ..
                    }) => {
                        unsupported_files.push(display);
                    }
                    Err(ServerError::Io(_)) => {
                        files_skipped = files_skipped.saturating_add(1);
                    }
                    Err(err) => return Err(err),
                }
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
            Ok(attach_query_notes(
                Response::ok(result, freshness),
                any_freshly_processed,
                &[],
                false,
            ))
        })
        .await
}

// --- shared plumbing ---------------------------------------------------

struct QueryRun {
    outcome: ModuleQueryRun,
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
    imports: Vec<String>,
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

async fn run_module_query(project: &Arc<LeanProject>, path: &Path, query: LeanWorkerModuleQuery) -> Result<QueryRun> {
    let input = read_query_file(project.canonical_root(), path)?;
    let key = ModuleQueryKey::from_query(&query);
    if let Some(outcome) = project.module_query_cache().get(&input.resolved, input.hash, &key) {
        return Ok(QueryRun {
            outcome: route_query_outcome(outcome, false),
        });
    }

    let outcome = process_module_query(project, input.source, query).await?;
    project
        .module_query_cache()
        .insert(input.resolved, input.hash, key, outcome.clone());
    Ok(QueryRun {
        outcome: route_query_outcome(outcome, true),
    })
}

async fn run_module_query_batch(
    project: &Arc<LeanProject>,
    path: &Path,
    selectors: Vec<LeanWorkerModuleQuerySelector>,
    budgets: LeanWorkerOutputBudgets,
) -> Result<BatchRun> {
    let input = read_query_file(project.canonical_root(), path)?;
    let file_label = input.resolved.to_string_lossy().into_owned();
    let outcome = process_module_query_batch(project, input.source, file_label, selectors, budgets).await?;
    Ok(BatchRun {
        outcome: route_batch_outcome(outcome),
        imports: input.imports,
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

struct QueryFile {
    resolved: PathBuf,
    hash: [u8; 32],
    imports: Vec<String>,
    source: String,
}

fn read_query_file(root: &Path, path: &Path) -> Result<QueryFile> {
    let resolved = resolve_path(root, path).canonicalize().map_err(ServerError::Io)?;
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = hash_bytes(&bytes);
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    let imports = header_imports(&source);
    Ok(QueryFile {
        resolved,
        hash,
        imports,
        source,
    })
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

async fn process_module_query(
    project: &Arc<LeanProject>,
    source: String,
    query: LeanWorkerModuleQuery,
) -> Result<LeanWorkerModuleQueryOutcome> {
    let imports = session_imports(header_imports(&source));
    project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            session
                .process_module_query(&source, query, &LeanWorkerElabOptions::new(), None, None)
                .map_err(map_worker_err)
        })
        .await
}

async fn process_module_query_batch(
    project: &Arc<LeanProject>,
    source: String,
    file_label: String,
    selectors: Vec<LeanWorkerModuleQuerySelector>,
    budgets: LeanWorkerOutputBudgets,
) -> Result<LeanWorkerModuleQueryBatchOutcome> {
    let imports = session_imports(header_imports(&source));
    project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let options = LeanWorkerElabOptions::new().file_label(&file_label);
            session
                .process_module_query_batch(&source, &selectors, &budgets, &options, None, None)
                .map_err(map_worker_err)
        })
        .await
}

enum ProjectedBatchItem {
    Ok { id: String, result: LeanQueryProjection },
    Unavailable { id: String, message: String },
    BudgetExceeded { id: String, message: String },
}

impl ProjectedBatchItem {
    fn into_item(self) -> (String, LeanQueryItem) {
        match self {
            Self::Ok { id, result } => (id, LeanQueryItem::Ok { result }),
            Self::Unavailable { id, message } => (id, LeanQueryItem::Unavailable { message }),
            Self::BudgetExceeded { id, message } => (id, LeanQueryItem::BudgetExceeded { message }),
        }
    }
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

fn project_batch_result(result: LeanWorkerModuleQueryBatchResult, file: Option<&str>) -> LeanQueryProjection {
    match result {
        LeanWorkerModuleQueryBatchResult::Diagnostics(failure) => {
            LeanQueryProjection::Diagnostics(diagnostics_block(project_failure(&failure)))
        }
        LeanWorkerModuleQueryBatchResult::ProofState(result) => {
            LeanQueryProjection::ProofState(project_proof_state_result(result))
        }
        LeanWorkerModuleQueryBatchResult::TypeAt(result) => LeanQueryProjection::TypeAt(project_type_at_result(result)),
        LeanWorkerModuleQueryBatchResult::References(result) => {
            let display = file.unwrap_or("");
            LeanQueryProjection::References(ReferencesProjection {
                references: result
                    .references
                    .iter()
                    .map(|node| project_reference(display, node))
                    .collect(),
                truncated: result.truncated,
            })
        }
        LeanWorkerModuleQueryBatchResult::DeclarationTarget(result) => {
            LeanQueryProjection::DeclarationTarget(project_declaration_target_result(result))
        }
        LeanWorkerModuleQueryBatchResult::SurroundingDeclaration(result) => {
            LeanQueryProjection::SurroundingDeclaration(project_surrounding_declaration_result(result))
        }
        _ => LeanQueryProjection::Diagnostics(DiagnosticsBlock {
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
        _ => ProofStateProjection::Unavailable {
            message: "worker returned an unknown proof-state result".to_owned(),
        },
    }
}

fn project_proof_state_info(info: LeanWorkerProofStateInfo) -> ProofStateContext {
    ProofStateContext {
        declaration_name: info.declaration_name,
        namespace_name: info.namespace_name,
        safe_edit: info.safe_edit.map(project_declaration_target_info),
        span: span_of_module(info.span),
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

fn duplicate_selector_id(selectors: &[LeanQuerySelector]) -> Option<String> {
    let mut seen = BTreeSet::new();
    for selector in selectors {
        let id = selector.id();
        if !seen.insert(id.to_owned()) {
            return Some(id.to_owned());
        }
    }
    None
}

fn validate_lean_query_selectors(selectors: &[LeanQuerySelector]) -> Option<String> {
    if selectors.is_empty() {
        return Some("selectors must not be empty".to_owned());
    }
    duplicate_selector_id(selectors).map(|id| format!("selector id `{id}` appears more than once"))
}

fn sort_diagnostics(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort_by_key(|d| d.position.as_ref().map_or((u32::MAX, u32::MAX), |p| (p.line, p.column)));
    diagnostics
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
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
    if !missing_imports.is_empty() {
        response.warnings.push(format!(
            "file header referenced imports not present in the opened session: {}",
            missing_imports.join(", ")
        ));
    }
    response
}

fn attach_batch_query_notes<T>(
    mut response: Response<T>,
    facts: &ModuleQueryFacts,
    missing_imports: &[String],
) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    response
        .next_actions
        .push(format!("worker module snapshot cache status: {}", facts.cache_status));
    if !missing_imports.is_empty() {
        response.warnings.push(format!(
            "file header referenced imports not present in the opened session: {}",
            missing_imports.join(", ")
        ));
    }
    response
}

fn enumerate_lean_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| entry.file_name().to_str().is_none_or(|name| !is_ignored_dir(name)))
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "lean"))
        .map(|entry| entry.into_path())
        .collect()
}

fn header_imports(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.split_once("--").map_or(line, |(before, _)| before);
            let mut words = line.split_whitespace();
            let mut token = words.next()?;
            if token == "public" {
                token = words.next()?;
            }
            if token == "meta" {
                token = words.next()?;
            }
            if token != "import" {
                return None;
            }
            if words.clone().next() == Some("all") {
                let _ = words.next();
            }
            words.next().map(str::to_owned)
        })
        .collect()
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
        BatchQueryRun, DeclarationTargetProjection, DiagnosticSummary, DiagnosticsBlock, LeanQueryRequest,
        LeanQueryResult, LeanQuerySelector, ProofStateRequest, ProofStateResult, RenderedText, SourceSpan,
        SurroundingDeclarationProjection, cache_status_label, duplicate_selector_id, expert_query_budgets,
        header_imports, project_query_facts, proof_agent_budgets, read_query_file, reference_query_budgets,
        route_batch_outcome, validate_lean_query_selectors,
    };

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
            serde_json::from_str(r#"{"file":"Foo/Bar.lean","line":7,"column":3}"#).unwrap();
        assert_eq!(request.line, 7);
        assert_eq!(request.column, 3);
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
    fn proof_state_context_serializes_flattened_workflow_fields() {
        let result = ProofStateResult::Context {
            diagnostics: DiagnosticsBlock {
                summary: DiagnosticSummary::default(),
                diagnostics: Vec::new(),
                truncated: false,
            },
            declaration_name: Some("Demo.proof".to_owned()),
            namespace_name: Some("Demo".to_owned()),
            span: Some(SourceSpan {
                start_line: 3,
                start_column: 5,
                end_line: 3,
                end_column: 12,
            }),
            goals_before: vec!["⊢ True".to_owned()],
            goals_after: Vec::new(),
            locals: Vec::new(),
            expected_type: Some(RenderedText {
                value: "True".to_owned(),
                truncated: false,
            }),
            safe_edit: None,
            truncated: false,
            target_declaration: Some(Box::new(DeclarationTargetProjection::NotFound)),
            surrounding_declaration: Some(Box::new(SurroundingDeclarationProjection::None)),
            term: None,
            total_truncated: false,
            unavailable: Vec::new(),
            budget_exceeded: Vec::new(),
            query_facts: Box::new(project_query_facts(worker_facts(LeanWorkerModuleCacheStatus::Miss))),
        };

        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["status"], "context");
        assert_eq!(value["declaration_name"], "Demo.proof");
        assert_eq!(value["namespace_name"], "Demo");
        assert_eq!(value["span"]["start_line"], 3);
        assert_eq!(value["goals_before"][0], "⊢ True");
        assert_eq!(value["expected_type"]["value"], "True");
        assert_eq!(value["target_declaration"]["status"], "not_found");
        assert!(value.get("proof_state").is_none());
        assert!(value.get("declaration_target").is_none());
    }

    #[test]
    fn lean_query_request_round_trips_typed_selectors() {
        let request: LeanQueryRequest = serde_json::from_str(
            r#"{"file":"Foo.lean","selectors":[
                {"selector":"diagnostics","id":"d"},
                {"selector":"proof_state","id":"p","line":4,"column":2},
                {"selector":"type_at","id":"t","line":5,"column":9},
                {"selector":"references","id":"r","name":"Nat.add"},
                {"selector":"declaration_target","id":"target","line":5,"column":9},
                {"selector":"surrounding_declaration","id":"around","line":5,"column":9}
            ]}"#,
        )
        .unwrap();
        assert_eq!(request.selectors.len(), 6);
        assert!(matches!(request.selectors[0], LeanQuerySelector::Diagnostics { .. }));
    }

    #[test]
    fn lean_query_selector_validation_rejects_empty_and_duplicate_ids() {
        let empty: Vec<LeanQuerySelector> = Vec::new();
        assert_eq!(duplicate_selector_id(&empty), None);
        assert_eq!(
            validate_lean_query_selectors(&empty),
            Some("selectors must not be empty".to_owned())
        );

        let selectors = vec![
            LeanQuerySelector::Diagnostics { id: "same".to_owned() },
            LeanQuerySelector::TypeAt {
                id: "same".to_owned(),
                line: 1,
                column: 1,
            },
        ];
        assert_eq!(duplicate_selector_id(&selectors), Some("same".to_owned()));
        assert_eq!(
            validate_lean_query_selectors(&selectors),
            Some("selector id `same` appears more than once".to_owned())
        );
    }

    #[test]
    fn lean_query_result_serialises_status_tag() {
        let unsupported = serde_json::to_string(&LeanQueryResult::Unsupported).unwrap();
        assert_eq!(unsupported, r#"{"status":"unsupported"}"#);

        let invalid = serde_json::to_string(&LeanQueryResult::InvalidSelectors {
            message: "selectors must not be empty".to_owned(),
        })
        .unwrap();
        assert_eq!(
            invalid,
            r#"{"status":"invalid_selectors","message":"selectors must not be empty"}"#
        );
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
