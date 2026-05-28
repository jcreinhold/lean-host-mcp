//! Pure data-shuffle projections from `lean-rs-worker` types into the
//! MCP-stable wire shapes.
//!
//! No Lean dependency past `Serialize + Deserialize` round-trips, no
//! `LeanRuntime` handle—these can be called from any thread and
//! serialise straight into the JSON envelope.
//!
//! All projection *types* and *helper functions* live here. Tool handlers
//! import the wire types from this module; the [`project`](crate::project)
//! module calls the helpers from inside the worker-actor closure so the
//! values returned to the tool layer already wear the wire shape.

// `map_worker_err` is the natural `.map_err(...)` callable for the
// closures the tool handlers ship to the worker actor. The
// pass-by-value shape is forced by `Result::map_err`. Several other
// projection helpers consume their argument; the lint is noise here.
#![allow(clippy::needless_pass_by_value)]

use lean_rs_worker_parent::{
    LeanWorkerDeclarationFlags, LeanWorkerDeclarationInspection as WorkerDeclarationInspection,
    LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationProofSearchFacts, LeanWorkerDeclarationRow,
    LeanWorkerDeclarationSearchFacts, LeanWorkerDeclarationSearchPruning, LeanWorkerDeclarationSearchResult,
    LeanWorkerDeclarationSearchRow, LeanWorkerDeclarationSearchTimings, LeanWorkerDeclarationTargetInfo,
    LeanWorkerDeclarationVerificationFacts, LeanWorkerDeclarationVerificationResult,
    LeanWorkerDeclarationVerificationStatus, LeanWorkerDiagnostic, LeanWorkerElabFailure, LeanWorkerElabResult,
    LeanWorkerError, LeanWorkerKernelResult, LeanWorkerKernelStatus, LeanWorkerMetaResult, LeanWorkerModuleSourceSpan,
    LeanWorkerProofAttemptEnvelope, LeanWorkerProofAttemptResult, LeanWorkerProofAttemptRow,
    LeanWorkerProofAttemptStatus, LeanWorkerRendered, LeanWorkerRenderedInfo, LeanWorkerRendering,
    LeanWorkerSourceRange,
};

use crate::error::ServerError;

/// Source-range projection with public fields mirroring `LeanWorkerSourceRange`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SourceRange {
    pub file: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Position {
    pub line: u32,
    pub column: u32,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

/// Wire-stable severity classification. The three Lean severities map to
/// `snake_case` strings so the field is uniform with every other status
/// discriminant in the server.
#[derive(Debug, Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// The worker layer emits severity as a snake-case string; unknown
    /// values map to `Info` rather than blocking the response.
    fn from_worker(s: &str) -> Self {
        match s {
            "error" => Self::Error,
            "warning" => Self::Warning,
            _ => Self::Info,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub position: Option<Position>,
    /// Real source path when Lean attached one. Omitted for the synthetic
    /// `<elaborate>` label that elaboration-buffer calls always produce;
    /// the caller already knows which file they asked about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Structured failure payload: the projection of `LeanWorkerElabFailure`
/// sent over JSON. Failure is part of a successful tool call; this is never
/// an MCP error.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ElabFailure {
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ElabSuccess {
    pub ok: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct KernelOutcome {
    /// One of `Checked`, `Rejected`, `Unavailable`, or `Unsupported`.
    pub status: String,
    pub summary: Option<KernelSummary>,
    pub failure: Option<ElabFailure>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct KernelSummary {
    pub declaration_name: String,
    pub kind: String,
    /// Pretty-printed declaration type via Lean's `PrettyPrinter`
    /// (notation-aware), as the worker's `LeanWorkerKernelSummary` reports
    /// it.
    pub type_signature: String,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct MetaOutcome {
    /// One of `Ok`, `Failed`, `TimeoutOrHeartbeat`, or `Unsupported`.
    pub status: String,
    pub rendered: Option<String>,
    pub definitionally_equal: Option<bool>,
    pub failure: Option<ElabFailure>,
    /// Set when the worker reported `LeanWorkerRendering::Raw`: the
    /// optional `meta_pp_expr` shim was missing or reported `Unsupported`,
    /// and the rendering fell back to `Expr.toString`. Internal; tool
    /// handlers translate this into an envelope warning, and the field
    /// never serialises.
    #[serde(skip)]
    #[schemars(skip)]
    pub raw_fallback_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationRow {
    pub name: String,
    pub kind: String,
    /// Pretty-printed type signature as the worker's
    /// `LeanWorkerDeclarationRow` reports it. `None` only when the
    /// declaration has no recoverable type (typically internal artifacts).
    pub type_signature: Option<String>,
    pub source: Option<SourceRange>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct RenderedText {
    pub value: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ModuleSourceSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ProofActionDeclarationTarget {
    pub short_name: String,
    pub declaration_name: String,
    pub namespace_name: String,
    pub declaration_kind: String,
    pub declaration_span: ModuleSourceSpan,
    pub name_span: ModuleSourceSpan,
    pub body_span: ModuleSourceSpan,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationFlags {
    pub is_private: bool,
    pub is_generated: bool,
    pub is_internal: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationSummary {
    pub name: String,
    pub kind: String,
    pub module: Option<String>,
    pub source: Option<SourceRange>,
    pub match_reason: String,
    pub score: i32,
    pub rank: usize,
    pub flags: DeclarationFlags,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationSearchPruning {
    pub stage: String,
    pub reason: String,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationSearchTimings {
    pub scan_micros: u64,
    pub rank_micros: u64,
    pub source_micros: u64,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationSearchFacts {
    pub declarations_scanned: usize,
    pub after_name_filter: usize,
    pub after_kind_filter: usize,
    pub after_required_constants_filter: usize,
    pub after_conclusion_filter: usize,
    pub after_scope_filter: usize,
    pub source_lookups: usize,
    pub broad_pruning: Vec<DeclarationSearchPruning>,
    pub truncated: bool,
    pub timings: DeclarationSearchTimings,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationSearchResult {
    pub declarations: Vec<DeclarationSummary>,
    pub truncated: bool,
    pub facts: DeclarationSearchFacts,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "proof-search booleans mirror independent lean-rs wire facts"
)]
pub struct DeclarationProofSearchFacts {
    pub is_simp: bool,
    pub is_rw_candidate: bool,
    pub is_instance: bool,
    pub is_class: bool,
    pub class_name: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationInspection {
    pub name: String,
    pub kind: String,
    pub module: Option<String>,
    pub source: Option<SourceRange>,
    pub statement: Option<RenderedText>,
    pub docstring: Option<RenderedText>,
    pub attributes: Vec<String>,
    pub proof_search: DeclarationProofSearchFacts,
    pub flags: DeclarationFlags,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationInspectionCandidate {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeclarationInspectionResult {
    Found {
        declaration: Box<DeclarationInspection>,
    },
    NotFound {
        name: Option<String>,
    },
    Ambiguous {
        candidates: Vec<DeclarationInspectionCandidate>,
    },
    Unsupported,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ProofAttemptCandidate {
    pub id: String,
    pub status: String,
    pub diagnostics: ElabFailure,
    pub downstream_diagnostics: ElabFailure,
    pub goals: Vec<RenderedText>,
    pub declaration: Option<ProofActionDeclarationTarget>,
    pub proof_position: Option<ProofPositionSummary>,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ProofPositionSummary {
    pub index: u32,
    pub tactic: RenderedText,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ProofAttemptEnvelope {
    pub candidates: Vec<ProofAttemptCandidate>,
    pub candidate_limit: u32,
    pub candidates_truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProofAttemptResult {
    Ok {
        result: ProofAttemptEnvelope,
        imports: Vec<String>,
    },
    MissingImports {
        result: ProofAttemptEnvelope,
        imports: Vec<String>,
        missing: Vec<String>,
    },
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "verification booleans mirror independent lean-rs wire facts for proof policy decisions"
)]
pub struct DeclarationVerificationFacts {
    pub target: Option<ProofActionDeclarationTarget>,
    pub diagnostics: ElabFailure,
    pub unresolved_goals: Vec<RenderedText>,
    pub contains_sorry: bool,
    pub contains_admit: bool,
    pub contains_sorry_ax: bool,
    pub axioms: Vec<String>,
    pub axioms_truncated: bool,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeclarationVerificationResult {
    Ok {
        verification_status: String,
        facts: Box<DeclarationVerificationFacts>,
        imports: Vec<String>,
    },
    MissingImports {
        verification_status: String,
        facts: Box<DeclarationVerificationFacts>,
        imports: Vec<String>,
        missing: Vec<String>,
    },
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

// --- projection helpers -------------------------------------------------

pub fn project_diagnostic(d: &LeanWorkerDiagnostic) -> Diagnostic {
    let position = d.line.map(|line| Position {
        line,
        column: d.column.unwrap_or(0),
        end_line: d.end_line,
        end_column: d.end_column,
    });
    Diagnostic {
        severity: Severity::from_worker(&d.severity),
        message: d.message.clone(),
        position,
        file: meaningful_file_label(&d.file_label),
    }
}

pub fn project_failure(failure: &LeanWorkerElabFailure) -> ElabFailure {
    ElabFailure {
        diagnostics: failure.diagnostics.iter().map(project_diagnostic).collect(),
        truncated: failure.truncated,
    }
}

pub(crate) fn project_source_range(range: LeanWorkerSourceRange) -> SourceRange {
    SourceRange {
        file: range.file,
        start_line: range.start_line,
        start_column: range.start_column,
        end_line: range.end_line,
        end_column: range.end_column,
    }
}

/// Strip Lean's synthetic source label so it never reaches the wire. Every
/// call that elaborates a string buffer (rather than a file on disk) gets
/// labelled `<elaborate>`, which is never actionable for the caller.
fn meaningful_file_label(label: &str) -> Option<String> {
    if label.is_empty() || label == "<elaborate>" || label.starts_with('<') {
        None
    } else {
        Some(label.to_owned())
    }
}

/// # Errors
///
/// Returns the projected [`ElabFailure`] in the error arm when the
/// upstream `LeanWorkerElabResult.success` is `false`. Never an
/// infrastructure error.
pub fn project_elab_result(result: LeanWorkerElabResult) -> std::result::Result<ElabSuccess, ElabFailure> {
    if result.success {
        Ok(ElabSuccess { ok: true })
    } else {
        Err(ElabFailure {
            diagnostics: result.diagnostics.iter().map(project_diagnostic).collect(),
            truncated: result.truncated,
        })
    }
}

pub fn project_kernel_result(result: LeanWorkerKernelResult) -> KernelOutcome {
    let status = match result.status {
        LeanWorkerKernelStatus::Checked => "Checked",
        LeanWorkerKernelStatus::Rejected => "Rejected",
        LeanWorkerKernelStatus::Unavailable => "Unavailable",
        LeanWorkerKernelStatus::Unsupported => "Unsupported",
        _ => "Unsupported",
    };
    let summary = result.summary.map(|s| KernelSummary {
        declaration_name: s.declaration_name,
        kind: s.kind,
        type_signature: s.type_signature,
    });
    let failure = if matches!(result.status, LeanWorkerKernelStatus::Checked) {
        None
    } else {
        Some(ElabFailure {
            diagnostics: result.diagnostics.iter().map(project_diagnostic).collect(),
            truncated: result.truncated,
        })
    };
    KernelOutcome {
        status: status.to_owned(),
        summary,
        failure,
    }
}

pub fn project_meta_rendered(result: LeanWorkerMetaResult<LeanWorkerRendered>) -> MetaOutcome {
    match result {
        LeanWorkerMetaResult::Ok { value } => MetaOutcome {
            status: "Ok".into(),
            rendered: Some(value.value),
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: matches!(value.rendering, LeanWorkerRendering::Raw),
        },
        LeanWorkerMetaResult::Failed { failure } => meta_failure("Failed", &failure),
        LeanWorkerMetaResult::TimeoutOrHeartbeat { failure } => meta_failure("TimeoutOrHeartbeat", &failure),
        LeanWorkerMetaResult::Unsupported { failure } => meta_failure("Unsupported", &failure),
        _ => MetaOutcome {
            status: "Unsupported".into(),
            rendered: None,
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        },
    }
}

pub fn project_meta_bool(result: LeanWorkerMetaResult<bool>) -> MetaOutcome {
    match result {
        LeanWorkerMetaResult::Ok { value } => MetaOutcome {
            status: "Ok".into(),
            rendered: None,
            definitionally_equal: Some(value),
            failure: None,
            raw_fallback_used: false,
        },
        LeanWorkerMetaResult::Failed { failure } => meta_failure("Failed", &failure),
        LeanWorkerMetaResult::TimeoutOrHeartbeat { failure } => meta_failure("TimeoutOrHeartbeat", &failure),
        LeanWorkerMetaResult::Unsupported { failure } => meta_failure("Unsupported", &failure),
        _ => MetaOutcome {
            status: "Unsupported".into(),
            rendered: None,
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        },
    }
}

fn meta_failure(status: &str, failure: &LeanWorkerElabFailure) -> MetaOutcome {
    MetaOutcome {
        status: status.to_owned(),
        rendered: None,
        definitionally_equal: None,
        failure: Some(project_failure(failure)),
        raw_fallback_used: false,
    }
}

pub fn project_declaration_row(row: LeanWorkerDeclarationRow) -> DeclarationRow {
    DeclarationRow {
        name: row.name,
        kind: row.kind,
        type_signature: row.type_signature,
        source: row.source.map(project_source_range),
    }
}

pub(crate) fn project_rendered_info(info: LeanWorkerRenderedInfo) -> RenderedText {
    RenderedText {
        value: info.value,
        truncated: info.truncated,
    }
}

pub(crate) fn project_module_source_span(span: LeanWorkerModuleSourceSpan) -> ModuleSourceSpan {
    ModuleSourceSpan {
        start_line: span.start_line,
        start_column: span.start_column,
        end_line: span.end_line,
        end_column: span.end_column,
    }
}

pub(crate) fn project_proof_action_target(info: LeanWorkerDeclarationTargetInfo) -> ProofActionDeclarationTarget {
    ProofActionDeclarationTarget {
        short_name: info.short_name,
        declaration_name: info.declaration_name,
        namespace_name: info.namespace_name,
        declaration_kind: info.declaration_kind,
        declaration_span: project_module_source_span(info.declaration_span),
        name_span: project_module_source_span(info.name_span),
        body_span: project_module_source_span(info.body_span),
    }
}

pub(crate) fn project_declaration_flags(flags: LeanWorkerDeclarationFlags) -> DeclarationFlags {
    DeclarationFlags {
        is_private: flags.is_private,
        is_generated: flags.is_generated,
        is_internal: flags.is_internal,
    }
}

fn project_declaration_summary(row: LeanWorkerDeclarationSearchRow) -> DeclarationSummary {
    DeclarationSummary {
        name: row.name,
        kind: row.kind,
        module: row.module,
        source: row.source.map(project_source_range),
        match_reason: row.match_reason,
        score: row.score,
        rank: row.rank,
        flags: project_declaration_flags(row.flags),
    }
}

fn project_declaration_search_pruning(row: LeanWorkerDeclarationSearchPruning) -> DeclarationSearchPruning {
    DeclarationSearchPruning {
        stage: row.stage,
        reason: row.reason,
        count: row.count,
    }
}

fn project_declaration_search_timings(timings: LeanWorkerDeclarationSearchTimings) -> DeclarationSearchTimings {
    DeclarationSearchTimings {
        scan_micros: timings.scan_micros,
        rank_micros: timings.rank_micros,
        source_micros: timings.source_micros,
    }
}

fn project_declaration_search_facts(facts: LeanWorkerDeclarationSearchFacts) -> DeclarationSearchFacts {
    DeclarationSearchFacts {
        declarations_scanned: facts.declarations_scanned,
        after_name_filter: facts.after_name_filter,
        after_kind_filter: facts.after_kind_filter,
        after_required_constants_filter: facts.after_required_constants_filter,
        after_conclusion_filter: facts.after_conclusion_filter,
        after_scope_filter: facts.after_scope_filter,
        source_lookups: facts.source_lookups,
        broad_pruning: facts
            .broad_pruning
            .into_iter()
            .map(project_declaration_search_pruning)
            .collect(),
        truncated: facts.truncated,
        timings: project_declaration_search_timings(facts.timings),
    }
}

pub fn project_declaration_search(result: LeanWorkerDeclarationSearchResult) -> DeclarationSearchResult {
    DeclarationSearchResult {
        declarations: result
            .declarations
            .into_iter()
            .map(project_declaration_summary)
            .collect(),
        truncated: result.truncated,
        facts: project_declaration_search_facts(result.facts),
    }
}

pub fn project_declaration_inspection(result: LeanWorkerDeclarationInspectionResult) -> DeclarationInspectionResult {
    match result {
        LeanWorkerDeclarationInspectionResult::Found { declaration } => DeclarationInspectionResult::Found {
            declaration: Box::new(project_inspection(*declaration)),
        },
        LeanWorkerDeclarationInspectionResult::NotFound { name } => {
            DeclarationInspectionResult::NotFound { name: Some(name) }
        }
        LeanWorkerDeclarationInspectionResult::Unsupported => DeclarationInspectionResult::Unsupported,
        _ => DeclarationInspectionResult::Unsupported,
    }
}

pub fn project_proof_attempt(result: LeanWorkerProofAttemptResult) -> ProofAttemptResult {
    match result {
        LeanWorkerProofAttemptResult::Ok { result, imports } => ProofAttemptResult::Ok {
            result: project_proof_attempt_envelope(result),
            imports,
        },
        LeanWorkerProofAttemptResult::MissingImports {
            result,
            imports,
            missing,
        } => ProofAttemptResult::MissingImports {
            result: project_proof_attempt_envelope(result),
            imports,
            missing,
        },
        LeanWorkerProofAttemptResult::HeaderParseFailed { diagnostics } => ProofAttemptResult::HeaderParseFailed {
            diagnostics: project_failure(&diagnostics),
        },
        LeanWorkerProofAttemptResult::Unsupported => ProofAttemptResult::Unsupported,
        _ => ProofAttemptResult::Unsupported,
    }
}

pub(crate) fn project_proof_attempt_envelope(envelope: LeanWorkerProofAttemptEnvelope) -> ProofAttemptEnvelope {
    ProofAttemptEnvelope {
        candidates: envelope.candidates.into_iter().map(project_proof_attempt_row).collect(),
        candidate_limit: envelope.candidate_limit,
        candidates_truncated: envelope.candidates_truncated,
    }
}

pub(crate) fn project_proof_attempt_row(row: LeanWorkerProofAttemptRow) -> ProofAttemptCandidate {
    ProofAttemptCandidate {
        id: row.id,
        status: proof_attempt_status(row.status).to_owned(),
        diagnostics: project_failure(&row.diagnostics),
        downstream_diagnostics: project_failure(&row.downstream_diagnostics),
        goals: row.goals.into_iter().map(project_rendered_info).collect(),
        declaration: row.declaration.map(project_proof_action_target),
        proof_position: row.proof_position.map(|position| ProofPositionSummary {
            index: position.index,
            tactic: project_rendered_info(position.tactic),
        }),
        output_truncated: row.output_truncated,
    }
}

fn proof_attempt_status(status: LeanWorkerProofAttemptStatus) -> &'static str {
    match status {
        LeanWorkerProofAttemptStatus::Closed => "closed",
        LeanWorkerProofAttemptStatus::Progressed => "progressed",
        LeanWorkerProofAttemptStatus::Failed => "failed",
        LeanWorkerProofAttemptStatus::Timeout => "timeout",
        LeanWorkerProofAttemptStatus::BudgetExceeded => "budget_exceeded",
        LeanWorkerProofAttemptStatus::Unsupported => "unsupported",
        _ => "unsupported",
    }
}

pub fn project_declaration_verification(
    result: LeanWorkerDeclarationVerificationResult,
) -> DeclarationVerificationResult {
    match result {
        LeanWorkerDeclarationVerificationResult::Ok {
            verification_status,
            facts,
            imports,
        } => DeclarationVerificationResult::Ok {
            verification_status: verification_status_label(verification_status, &facts).to_owned(),
            facts: Box::new(project_declaration_verification_facts(*facts)),
            imports,
        },
        LeanWorkerDeclarationVerificationResult::MissingImports {
            verification_status,
            facts,
            imports,
            missing,
        } => DeclarationVerificationResult::MissingImports {
            verification_status: verification_status_label(verification_status, &facts).to_owned(),
            facts: Box::new(project_declaration_verification_facts(*facts)),
            imports,
            missing,
        },
        LeanWorkerDeclarationVerificationResult::HeaderParseFailed { diagnostics } => {
            DeclarationVerificationResult::HeaderParseFailed {
                diagnostics: project_failure(&diagnostics),
            }
        }
        LeanWorkerDeclarationVerificationResult::Unsupported => DeclarationVerificationResult::Unsupported,
        _ => DeclarationVerificationResult::Unsupported,
    }
}

fn project_declaration_verification_facts(
    facts: LeanWorkerDeclarationVerificationFacts,
) -> DeclarationVerificationFacts {
    DeclarationVerificationFacts {
        target: facts.target.map(project_proof_action_target),
        diagnostics: project_failure(&facts.diagnostics),
        unresolved_goals: facts.unresolved_goals.into_iter().map(project_rendered_info).collect(),
        contains_sorry: facts.contains_sorry,
        contains_admit: facts.contains_admit,
        contains_sorry_ax: facts.contains_sorry_ax,
        axioms: facts.axioms,
        axioms_truncated: facts.axioms_truncated,
        output_truncated: facts.output_truncated,
    }
}

fn verification_status_label(
    status: LeanWorkerDeclarationVerificationStatus,
    facts: &LeanWorkerDeclarationVerificationFacts,
) -> &'static str {
    match status {
        LeanWorkerDeclarationVerificationStatus::Accepted => "verified",
        LeanWorkerDeclarationVerificationStatus::Rejected if facts.contains_sorry => "has_sorry",
        LeanWorkerDeclarationVerificationStatus::Rejected if !facts.unresolved_goals.is_empty() => {
            "has_unresolved_goals"
        }
        LeanWorkerDeclarationVerificationStatus::Rejected if !facts.diagnostics.diagnostics.is_empty() => {
            "has_diagnostics"
        }
        LeanWorkerDeclarationVerificationStatus::Rejected => "has_diagnostics",
        LeanWorkerDeclarationVerificationStatus::NotFound => "not_found",
        LeanWorkerDeclarationVerificationStatus::Ambiguous => "ambiguous",
        LeanWorkerDeclarationVerificationStatus::Timeout => "timeout",
        LeanWorkerDeclarationVerificationStatus::BudgetExceeded => "budget_exceeded",
        LeanWorkerDeclarationVerificationStatus::Unsupported => "unsupported",
        _ => "unsupported",
    }
}

pub(crate) fn project_inspection(declaration: WorkerDeclarationInspection) -> DeclarationInspection {
    DeclarationInspection {
        name: declaration.name,
        kind: declaration.kind,
        module: declaration.module,
        source: declaration.source.map(project_source_range),
        statement: declaration.statement.map(project_rendered_info),
        docstring: declaration.docstring.map(project_rendered_info),
        attributes: declaration.attributes,
        proof_search: project_proof_search_facts(declaration.proof_search),
        flags: project_declaration_flags(declaration.flags),
    }
}

fn project_proof_search_facts(facts: LeanWorkerDeclarationProofSearchFacts) -> DeclarationProofSearchFacts {
    DeclarationProofSearchFacts {
        is_simp: facts.is_simp,
        is_rw_candidate: facts.is_rw_candidate,
        is_instance: facts.is_instance,
        is_class: facts.is_class,
        class_name: facts.class_name,
    }
}

/// Classify a worker-layer error.
///
/// Bootstrap failures map to `ServerError::BadProject`; everything else is
/// a Lean-domain runtime outcome and maps to `ServerError::Lean`. The
/// bootstrap-classification set is fixed by the worker contract.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "LeanWorkerError is upstream-evolving; everything outside the bootstrap-classification set maps to Lean for the MCP wire"
)]
pub fn map_worker_err(err: LeanWorkerError) -> ServerError {
    match err {
        LeanWorkerError::WorkerChildUnresolved { .. }
        | LeanWorkerError::WorkerChildNotExecutable { .. }
        | LeanWorkerError::Bootstrap { .. }
        | LeanWorkerError::CapabilityBuild { .. }
        | LeanWorkerError::Setup { .. }
        | LeanWorkerError::Handshake { .. }
        | LeanWorkerError::ChildPanicOrAbort { .. }
        | LeanWorkerError::ChildExited { .. }
        | LeanWorkerError::CapabilityMetadataMismatch { .. } => ServerError::BadProject(err.to_string()),
        _ => ServerError::Lean(err.to_string()),
    }
}
