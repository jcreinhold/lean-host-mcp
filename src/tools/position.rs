//! Position-based tools: `goal_at_position`, `type_at_position`,
//! `references_of_name`, `file_diagnostics`.
//!
//! All four project the same upstream value —
//! [`ProcessedFile`](lean_rs_host::host::process::ProcessedFile)—into a
//! tool-specific result enum. Repeated calls against the same source bytes
//! reuse a cached projection (see [`crate::cache::ProcessedFileCache`]).
//!
//! `file_diagnostics` is grouped here despite not taking a cursor: it shares
//! the cache-and-projection plumbing with its three siblings, so an agent's
//! typical "what's wrong; then probe the problem site" loop pays for the
//! elaboration once.
//!
//! The tools drive `LeanProject`'s worker actor with `process_module` —
//! the header-aware shim. The file's own `import` declarations are parsed by
//! Lean and validated against the server's open env; mismatch surfaces as a
//! `warnings` entry on the envelope, not as a silent empty result. A header
//! that fails to parse short-circuits to a `header_parse_failed` status
//! variant carrying the parser diagnostics.
//!
//! The shim is optional. A capability dylib built before `lean-rs` 0.1.4
//! lacks `process_module_with_info_tree`; each tool then answers with the
//! `Unsupported` result variant.

// Tool handlers consume their request structs (owned strings into the
// worker-actor channel); pass-by-value is intentional.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_worker::{
    LeanWorkerNameRef, LeanWorkerProcessModuleOutcome, LeanWorkerProcessedFile, LeanWorkerTacticInfo,
    LeanWorkerTermInfo,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::cache::{self, ProcessedFileCache, hash_bytes};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{Diagnostic, ElabFailure, Severity, project_failure};
use crate::tools::{ToolContext, is_ignored_dir, new_session_id};

/// Hard cap on the number of references aggregated in
/// [`references_of_name`]. Matches `project_scan`'s cap so the two
/// project-walking tools agree on the bound.
const MAX_REFERENCES: usize = 1000;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SourceSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

// --- goal_at_position --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GoalAtPositionRequest {
    /// Path to a `.lean` file. Resolved against `lake_root` if relative.
    pub file: PathBuf,
    /// 1-indexed line.
    pub line: u32,
    /// 1-indexed column.
    pub column: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GoalAtPositionResult {
    Goal {
        goals_before: Vec<String>,
        goals_after: Vec<String>,
        span: SourceSpan,
    },
    NoTacticContext,
    /// The file's header did not parse; the body was never elaborated.
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read, and propagates
/// `ServerError::Lean` from the underlying `process_module` call.
pub async fn goal_at_position(ctx: &ToolContext, req: GoalAtPositionRequest) -> Result<Response<GoalAtPositionResult>> {
    let freshness = ctx.freshness(&[], &new_session_id());
    let outcome = ensure_processed(ctx, &req.file).await?;
    let (file, fresh, missing) = match outcome {
        EnsureOutcome::Ready {
            file,
            freshly_processed,
            missing_imports,
        } => (file, freshly_processed, missing_imports),
        EnsureOutcome::HeaderParseFailed { diagnostics } => {
            return Ok(Response::ok(
                GoalAtPositionResult::HeaderParseFailed { diagnostics },
                freshness,
            ));
        }
        EnsureOutcome::Unsupported => {
            return Ok(Response::ok(GoalAtPositionResult::Unsupported, freshness));
        }
    };
    let result = match cache::tactic_at(&file, req.line, req.column) {
        Some(node) => GoalAtPositionResult::Goal {
            goals_before: node.goals_before.clone(),
            goals_after: node.goals_after.clone(),
            span: span_of_tactic(node),
        },
        None => GoalAtPositionResult::NoTacticContext,
    };
    Ok(attach_envelope_notes(Response::ok(result, freshness), fresh, &missing))
}

// --- type_at_position --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TypeAtPositionRequest {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TypeAtPositionResult {
    Term {
        /// `Expr.toString` of the elaborated expression at the cursor.
        expr: String,
        /// `Expr.toString` of the inferred type. Empty when Lean recorded
        /// the term but inference did not produce a type at that site.
        type_str: String,
        /// Set when the elaborator recorded an expected type (e.g., a
        /// coercion site).
        expected_type: Option<String>,
        span: SourceSpan,
    },
    NoTerm,
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

/// # Errors
///
/// As [`goal_at_position`].
pub async fn type_at_position(ctx: &ToolContext, req: TypeAtPositionRequest) -> Result<Response<TypeAtPositionResult>> {
    let freshness = ctx.freshness(&[], &new_session_id());
    let outcome = ensure_processed(ctx, &req.file).await?;
    let (file, fresh, missing) = match outcome {
        EnsureOutcome::Ready {
            file,
            freshly_processed,
            missing_imports,
        } => (file, freshly_processed, missing_imports),
        EnsureOutcome::HeaderParseFailed { diagnostics } => {
            return Ok(Response::ok(
                TypeAtPositionResult::HeaderParseFailed { diagnostics },
                freshness,
            ));
        }
        EnsureOutcome::Unsupported => {
            return Ok(Response::ok(TypeAtPositionResult::Unsupported, freshness));
        }
    };
    let result = match cache::term_at(&file, req.line, req.column) {
        Some(node) => TypeAtPositionResult::Term {
            expr: node.expr_str.clone(),
            type_str: node.type_str.clone(),
            expected_type: node.expected_type_str.clone(),
            span: span_of_term(node),
        },
        None => TypeAtPositionResult::NoTerm,
    };
    Ok(attach_envelope_notes(Response::ok(result, freshness), fresh, &missing))
}

// --- references_of_name ------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferencesOfNameRequest {
    /// Fully-qualified Lean name as the elaborator records it
    /// (e.g. `"Nat.add"`). No normalisation.
    pub name: String,
    /// Files to search. Empty = walk every `.lean` file under `lake_root`.
    #[serde(default)]
    pub files: Vec<PathBuf>,
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

/// Per-file sidebar entry for header parse failures during a walk.
///
/// The walk continues past these files; their diagnostics are surfaced here
/// so the caller can decide whether to act.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeaderParseFailedFile {
    pub file: String,
    pub diagnostics: ElabFailure,
}

/// Per-file sidebar entry for missing-import diagnostics during a walk.
///
/// The header parsed but referenced modules the session's open env does not
/// have. The file's body was still processed and any references it produced
/// are in the top-level `references` list.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MissingImportsFile {
    pub file: String,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReferencesOfNameResult {
    pub references: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Files whose capability dylib lacked the shim. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported_files: Vec<String>,
    /// Files whose header did not parse. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header_parse_failed_files: Vec<HeaderParseFailedFile>,
    /// Files with imports the server's env does not satisfy. Omitted when
    /// empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_imports_files: Vec<MissingImportsFile>,
}

/// # Errors
///
/// Returns `ServerError::Lean` if the underlying `process_module` call
/// fails for a non-`Unsupported`, non-`HeaderParseFailed`, non-`MissingImports`
/// reason. Files that cannot be read are skipped silently—same policy
/// as [`crate::tools::scan::project_scan`].
pub async fn references_of_name(
    ctx: &ToolContext,
    req: ReferencesOfNameRequest,
) -> Result<Response<ReferencesOfNameResult>> {
    let freshness = ctx.freshness(&[], &new_session_id());
    let root = ctx.project.canonical_root().to_path_buf();
    let files = if req.files.is_empty() {
        enumerate_lean_files(&root)
    } else {
        req.files.iter().map(|p| resolve_path(&root, p)).collect()
    };

    let mut hits: Vec<ReferenceHit> = Vec::new();
    let mut unsupported_files: Vec<String> = Vec::new();
    let mut header_parse_failed_files: Vec<HeaderParseFailedFile> = Vec::new();
    let mut missing_imports_files: Vec<MissingImportsFile> = Vec::new();
    let mut truncated = false;
    let mut any_freshly_processed = false;

    'outer: for path in files {
        let display = display_path(&root, &path);
        match ensure_processed(ctx, &path).await? {
            EnsureOutcome::Ready {
                file,
                freshly_processed,
                missing_imports,
            } => {
                if freshly_processed {
                    any_freshly_processed = true;
                }
                if !missing_imports.is_empty() {
                    missing_imports_files.push(MissingImportsFile {
                        file: display.clone(),
                        missing: missing_imports,
                    });
                }
                for node in cache::references_of(&file, &req.name) {
                    if hits.len() >= MAX_REFERENCES {
                        truncated = true;
                        break 'outer;
                    }
                    hits.push(project_reference(&display, node));
                }
            }
            EnsureOutcome::HeaderParseFailed { diagnostics } => {
                header_parse_failed_files.push(HeaderParseFailedFile {
                    file: display,
                    diagnostics,
                });
            }
            EnsureOutcome::Unsupported => {
                unsupported_files.push(display);
            }
        }
    }

    hits.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });

    let result = ReferencesOfNameResult {
        references: hits,
        truncated,
        unsupported_files,
        header_parse_failed_files,
        missing_imports_files,
    };
    // `references_of_name` accumulates per-file `MissingImports` into the
    // result sidebar rather than the envelope warnings—the single-file
    // tools surface it as a warning, but a project-wide walk against a
    // dozen mismatched files would drown the envelope.
    Ok(attach_envelope_notes(
        Response::ok(result, freshness),
        any_freshly_processed,
        &[],
    ))
}

// --- file_diagnostics --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FileDiagnosticsRequest {
    /// Path to a `.lean` file. Resolved against `lake_root` if relative.
    pub file: PathBuf,
}

/// Per-severity counts attached to every diagnostics-bearing variant. The
/// first read most callers do is "are there errors?"; surfacing the totals
/// here saves them iterating the list to find out.
#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
pub struct DiagnosticSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
}

impl DiagnosticSummary {
    fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut s = Self::default();
        for d in diagnostics {
            let bucket = match d.severity {
                Severity::Error => &mut s.errors,
                Severity::Warning => &mut s.warnings,
                Severity::Info => &mut s.info,
            };
            *bucket = bucket.saturating_add(1);
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FileDiagnosticsResult {
    /// Elaboration ran to completion. `summary` is the up-front
    /// errors/warnings/info tally; `diagnostics` is the full list, sorted
    /// by `(line, column)`. `truncated` is true only when Lean hit the
    /// diagnostic byte budget and the list is a prefix.
    ///
    /// Note: `summary.errors > 0` means the file has elaboration errors —
    /// `status: "elaborated"` only says we got far enough to collect them.
    Elaborated {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
    },
    /// The file's header did not parse; the body was never elaborated.
    /// Same shape as `Elaborated` so callers render one structure, not two.
    HeaderParseFailed {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
    },
    /// Capability dylib lacks `process_module_with_info_tree`.
    Unsupported,
}

/// # Errors
///
/// As [`goal_at_position`].
pub async fn file_diagnostics(
    ctx: &ToolContext,
    req: FileDiagnosticsRequest,
) -> Result<Response<FileDiagnosticsResult>> {
    let freshness = ctx.freshness(&[], &new_session_id());
    let outcome = ensure_processed(ctx, &req.file).await?;
    let (file, fresh, missing) = match outcome {
        EnsureOutcome::Ready {
            file,
            freshly_processed,
            missing_imports,
        } => (file, freshly_processed, missing_imports),
        EnsureOutcome::HeaderParseFailed { diagnostics } => {
            let ElabFailure { diagnostics, truncated } = diagnostics;
            let diagnostics = sort_diagnostics(diagnostics);
            let summary = DiagnosticSummary::from_diagnostics(&diagnostics);
            return Ok(Response::ok(
                FileDiagnosticsResult::HeaderParseFailed {
                    summary,
                    diagnostics,
                    truncated,
                },
                freshness,
            ));
        }
        EnsureOutcome::Unsupported => {
            return Ok(Response::ok(FileDiagnosticsResult::Unsupported, freshness));
        }
    };
    let projected = project_failure(&file.diagnostics);
    let diagnostics = sort_diagnostics(projected.diagnostics);
    let summary = DiagnosticSummary::from_diagnostics(&diagnostics);
    let result = FileDiagnosticsResult::Elaborated {
        summary,
        diagnostics,
        truncated: projected.truncated,
    };
    Ok(attach_envelope_notes(Response::ok(result, freshness), fresh, &missing))
}

/// Sort diagnostics by `(line, column)` so the wire order is deterministic
///—Lean's source order is usually but not always position-ordered.
fn sort_diagnostics(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort_by_key(|d| d.position.as_ref().map_or((u32::MAX, u32::MAX), |p| (p.line, p.column)));
    diagnostics
}

// --- shared plumbing ---------------------------------------------------

/// Outcome of running a file through the cache-or-process flow.
enum EnsureOutcome {
    Ready {
        file: Arc<LeanWorkerProcessedFile>,
        freshly_processed: bool,
        /// Header imports the session's open env does not have. Empty in
        /// the clean case. The body still ran against the env; the
        /// projection in `file` is real (possibly partial) data.
        missing_imports: Vec<String>,
    },
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

/// Read `path` from disk, hash, hit-or-fill the cache. On miss, dispatch
/// `process_module` to the project's worker actor and route the four-arm
/// outcome.
async fn ensure_processed(ctx: &ToolContext, path: &Path) -> Result<EnsureOutcome> {
    let resolved = resolve_path(ctx.project.canonical_root(), path);
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = hash_bytes(&bytes);
    let cache: &ProcessedFileCache = ctx.project.cache();
    if let Some(file) = cache.get(&resolved, hash) {
        return Ok(EnsureOutcome::Ready {
            file,
            freshly_processed: false,
            missing_imports: Vec::new(),
        });
    }
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    match process_module(ctx, source).await? {
        LeanWorkerProcessModuleOutcome::Ok { file, .. } => {
            let arc = Arc::new(file);
            cache.insert(resolved, hash, Arc::clone(&arc));
            Ok(EnsureOutcome::Ready {
                file: arc,
                freshly_processed: true,
                missing_imports: Vec::new(),
            })
        }
        LeanWorkerProcessModuleOutcome::MissingImports { file, missing, .. } => {
            let arc = Arc::new(file);
            cache.insert(resolved, hash, Arc::clone(&arc));
            Ok(EnsureOutcome::Ready {
                file: arc,
                freshly_processed: true,
                missing_imports: missing,
            })
        }
        LeanWorkerProcessModuleOutcome::HeaderParseFailed { diagnostics } => Ok(EnsureOutcome::HeaderParseFailed {
            diagnostics: project_failure(&diagnostics),
        }),
        LeanWorkerProcessModuleOutcome::Unsupported => Ok(EnsureOutcome::Unsupported),
    }
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

fn span_of_tactic(node: &LeanWorkerTacticInfo) -> SourceSpan {
    SourceSpan {
        start_line: node.start_line,
        start_column: node.start_column,
        end_line: node.end_line,
        end_column: node.end_column,
    }
}

fn span_of_term(node: &LeanWorkerTermInfo) -> SourceSpan {
    SourceSpan {
        start_line: node.start_line,
        start_column: node.start_column,
        end_line: node.end_line,
        end_column: node.end_column,
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

/// Header-aware module processing. Submits a closure to the project's
/// worker actor that opens a session against the project's default
/// imports, runs `process_module`, and returns the four-arm outcome.
async fn process_module(ctx: &ToolContext, source: String) -> Result<LeanWorkerProcessModuleOutcome> {
    let imports = ctx.project.default_imports().to_vec();
    ctx.project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(crate::projections::map_worker_err)?;
            session
                .process_module(&source, &lean_rs_worker::LeanWorkerElabOptions::new(), None, None)
                .map_err(crate::projections::map_worker_err)
        })
        .await
}

fn enumerate_lean_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("lean"))
        .map(|e| e.into_path())
        .collect()
}

/// Attach a cache-hint hint (if the file was freshly processed) and a
/// `MissingImports` warning (if non-empty). Used by `goal_at_position` and
/// `type_at_position`; `references_of_name` surfaces missing imports in
/// the result sidebar instead.
fn attach_envelope_notes<T>(resp: Response<T>, freshly_processed: bool, missing_imports: &[String]) -> Response<T>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    let resp = if freshly_processed {
        resp.hint("file processed and cached; subsequent position queries against the same contents reuse the cache")
    } else {
        resp
    };
    if missing_imports.is_empty() {
        resp
    } else {
        resp.warn(format!(
            "file imports modules the server's open env does not have: [{}]—projection may be partial; \
             relaunch the server with --imports to fix",
            missing_imports.join(", ")
        ))
    }
}
