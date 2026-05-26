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
    LeanWorkerDeclarationRow, LeanWorkerDiagnostic, LeanWorkerElabFailure, LeanWorkerElabResult, LeanWorkerError,
    LeanWorkerKernelResult, LeanWorkerKernelStatus, LeanWorkerMetaResult, LeanWorkerRendered, LeanWorkerRendering,
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

/// Re-export of [`LeanWorkerProcessedFile`] so callers that hold a cached
/// projection can pattern-match without importing `lean-rs-worker` directly.
pub use lean_rs_worker_parent::LeanWorkerProcessedFile as ProcessedFile;

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

fn project_source_range(range: LeanWorkerSourceRange) -> SourceRange {
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
