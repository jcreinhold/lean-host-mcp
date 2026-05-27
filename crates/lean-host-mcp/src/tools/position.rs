//! Bounded module-query tools: `file_diagnostics`, `goal_at_position`,
//! `type_at_position`, `references_in_file`, and `references_in_project`.
//!
//! Each tool calls `LeanWorkerSession::process_module_query` with the
//! narrow projection it needs. The server never requests, transports, or
//! caches a whole-file info tree.

// Tool handlers consume their request structs (owned strings into the
// worker-actor channel); pass-by-value is intentional.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_worker_parent::{
    LeanWorkerElabOptions, LeanWorkerGoalAtResult, LeanWorkerModuleQuery, LeanWorkerModuleQueryOutcome,
    LeanWorkerModuleQueryResult, LeanWorkerModuleSourceSpan, LeanWorkerNameRef, LeanWorkerRenderedInfo,
    LeanWorkerTypeAtResult,
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
use crate::tools::{ToolContext, freshness_for, is_ignored_dir, session_imports};

/// Hard cap on project-wide reference aggregation. File-local reference
/// queries are also bounded by the upstream projection.
const MAX_REFERENCES: usize = 1000;

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

// --- goal_at_position --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GoalAtPositionRequest {
    /// Path to a `.lean` file. Resolved against the resolved project root
    /// if relative.
    pub file: PathBuf,
    /// 1-indexed line.
    pub line: u32,
    /// 1-indexed column.
    pub column: u32,
    /// Optional explicit project (absolute path to Lake root). When
    /// omitted, the server resolves via env -> cwd-walk -> config default.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GoalAtPositionResult {
    Goal {
        goals_before: Vec<String>,
        goals_after: Vec<String>,
        span: SourceSpan,
        truncated: bool,
    },
    NoTacticContext,
    HeaderParseFailed {
        diagnostics: ElabFailure,
    },
    Unsupported,
}

/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read, and propagates
/// `ServerError::Lean` from the underlying `process_module_query` call.
pub async fn goal_at_position(ctx: &ToolContext, req: GoalAtPositionRequest) -> Result<Response<GoalAtPositionResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &[]);
            let query = LeanWorkerModuleQuery::GoalAt {
                line: req.line,
                column: req.column,
            };
            let outcome = run_module_query(&project, &req.file, query).await?;
            let (result, fresh, missing) = match outcome {
                ModuleQueryRun::Ready {
                    result: LeanWorkerModuleQueryResult::GoalAt(result),
                    freshly_processed,
                    missing_imports,
                } => {
                    let projected = match result {
                        LeanWorkerGoalAtResult::Goal {
                            span,
                            goals_before,
                            goals_after,
                            truncated,
                        } => GoalAtPositionResult::Goal {
                            goals_before,
                            goals_after,
                            span: span_of_module(span),
                            truncated,
                        },
                        LeanWorkerGoalAtResult::NoTacticContext => GoalAtPositionResult::NoTacticContext,
                        _ => GoalAtPositionResult::Unsupported,
                    };
                    (projected, freshly_processed, missing_imports)
                }
                ModuleQueryRun::Ready {
                    freshly_processed,
                    missing_imports,
                    ..
                } => (GoalAtPositionResult::Unsupported, freshly_processed, missing_imports),
                ModuleQueryRun::HeaderParseFailed { diagnostics } => {
                    return Ok(Response::ok(
                        GoalAtPositionResult::HeaderParseFailed { diagnostics },
                        freshness,
                    ));
                }
                ModuleQueryRun::Unsupported => {
                    return Ok(Response::ok(GoalAtPositionResult::Unsupported, freshness));
                }
            };
            Ok(attach_query_notes(Response::ok(result, freshness), fresh, &missing))
        })
        .await
}

// --- type_at_position --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TypeAtPositionRequest {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TypeAtPositionResult {
    Term {
        /// Bounded rendering of the elaborated expression at the cursor.
        expr: RenderedText,
        /// Bounded rendering of the inferred type.
        type_str: RenderedText,
        /// Set when the elaborator recorded an expected type.
        expected_type: Option<RenderedText>,
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
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &[]);
            let query = LeanWorkerModuleQuery::TypeAt {
                line: req.line,
                column: req.column,
            };
            let outcome = run_module_query(&project, &req.file, query).await?;
            let (result, fresh, missing) = match outcome {
                ModuleQueryRun::Ready {
                    result: LeanWorkerModuleQueryResult::TypeAt(result),
                    freshly_processed,
                    missing_imports,
                } => {
                    let projected = match result {
                        LeanWorkerTypeAtResult::Term {
                            span,
                            expr,
                            type_str,
                            expected_type,
                        } => TypeAtPositionResult::Term {
                            expr: rendered_text(expr),
                            type_str: rendered_text(type_str),
                            expected_type: expected_type.map(rendered_text),
                            span: span_of_module(span),
                        },
                        LeanWorkerTypeAtResult::NoTerm => TypeAtPositionResult::NoTerm,
                        _ => TypeAtPositionResult::Unsupported,
                    };
                    (projected, freshly_processed, missing_imports)
                }
                ModuleQueryRun::Ready {
                    freshly_processed,
                    missing_imports,
                    ..
                } => (TypeAtPositionResult::Unsupported, freshly_processed, missing_imports),
                ModuleQueryRun::HeaderParseFailed { diagnostics } => {
                    return Ok(Response::ok(
                        TypeAtPositionResult::HeaderParseFailed { diagnostics },
                        freshness,
                    ));
                }
                ModuleQueryRun::Unsupported => {
                    return Ok(Response::ok(TypeAtPositionResult::Unsupported, freshness));
                }
            };
            Ok(attach_query_notes(Response::ok(result, freshness), fresh, &missing))
        })
        .await
}

// --- references --------------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferencesInFileRequest {
    /// Path to a `.lean` file. Resolved against the resolved project root
    /// if relative.
    pub file: PathBuf,
    /// Fully-qualified Lean name as the elaborator records it
    /// (e.g. `"Nat.add"`). No normalisation.
    pub name: String,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReferencesInProjectRequest {
    /// Fully-qualified Lean name as the elaborator records it
    /// (e.g. `"Nat.add"`). No normalisation.
    pub name: String,
    /// Files to search. Empty = explicitly walk every `.lean` file under
    /// the resolved project root.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Maximum references to return. Values above the server cap are
    /// clamped to the cap.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
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
pub struct ReferencesInFileResult {
    pub references: Vec<ReferenceHit>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReferencesInProjectResult {
    pub references: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub files_scanned: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported_files: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header_parse_failed_files: Vec<HeaderParseFailedFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_imports_files: Vec<MissingImportsFile>,
}

/// # Errors
///
/// As [`goal_at_position`].
pub async fn references_in_file(
    ctx: &ToolContext,
    req: ReferencesInFileRequest,
) -> Result<Response<ReferencesInFileResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &[]);
            let root = project.canonical_root().to_path_buf();
            let resolved = resolve_path(&root, &req.file);
            let display = display_path(&root, &resolved);
            let query = LeanWorkerModuleQuery::References { name: req.name };
            let outcome = run_module_query(&project, &resolved, query).await?;
            match outcome {
                ModuleQueryRun::Ready {
                    result: LeanWorkerModuleQueryResult::References(result),
                    freshly_processed,
                    missing_imports,
                } => {
                    let references = result
                        .references
                        .iter()
                        .map(|node| project_reference(&display, node))
                        .collect();
                    let response = Response::ok(
                        ReferencesInFileResult {
                            references,
                            truncated: result.truncated,
                        },
                        freshness,
                    );
                    Ok(attach_query_notes(response, freshly_processed, &missing_imports))
                }
                ModuleQueryRun::Ready {
                    freshly_processed,
                    missing_imports,
                    ..
                } => Ok(attach_query_notes(
                    Response::ok(
                        ReferencesInFileResult {
                            references: Vec::new(),
                            truncated: false,
                        },
                        freshness,
                    ),
                    freshly_processed,
                    &missing_imports,
                )),
                ModuleQueryRun::HeaderParseFailed { diagnostics } => {
                    let mut response = Response::ok(
                        ReferencesInFileResult {
                            references: Vec::new(),
                            truncated: false,
                        },
                        freshness,
                    );
                    response
                        .warnings
                        .push("file header did not parse; no references were collected".to_owned());
                    response.warnings.push(format!(
                        "header diagnostics available from file_diagnostics: {}",
                        diagnostics.diagnostics.len()
                    ));
                    Ok(response)
                }
                ModuleQueryRun::Unsupported => Ok(Response::ok(
                    ReferencesInFileResult {
                        references: Vec::new(),
                        truncated: false,
                    },
                    freshness,
                )),
            }
        })
        .await
}

/// # Errors
///
/// Returns `ServerError::Lean` if an underlying file query fails for an
/// infrastructure reason. Files that cannot be read are skipped silently
/// (same policy as [`crate::tools::scan::project_scan`]).
pub async fn references_in_project(
    ctx: &ToolContext,
    req: ReferencesInProjectRequest,
) -> Result<Response<ReferencesInProjectResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &[]);
            let root = project.canonical_root().to_path_buf();
            let files = if req.files.is_empty() {
                enumerate_lean_files(&root)
            } else {
                req.files.iter().map(|p| resolve_path(&root, p)).collect()
            };
            let limit = req.limit.unwrap_or(MAX_REFERENCES).min(MAX_REFERENCES);

            let mut hits: Vec<ReferenceHit> = Vec::new();
            let mut unsupported_files: Vec<String> = Vec::new();
            let mut header_parse_failed_files: Vec<HeaderParseFailedFile> = Vec::new();
            let mut missing_imports_files: Vec<MissingImportsFile> = Vec::new();
            let mut truncated = false;
            let mut files_scanned = 0usize;
            let mut any_freshly_processed = false;

            'outer: for path in files {
                let display = display_path(&root, &path);
                let query = LeanWorkerModuleQuery::References { name: req.name.clone() };
                match run_module_query(&project, &path, query).await {
                    Ok(ModuleQueryRun::Ready {
                        result: LeanWorkerModuleQueryResult::References(result),
                        freshly_processed,
                        missing_imports,
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
                    Ok(ModuleQueryRun::Ready {
                        freshly_processed,
                        missing_imports,
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
                    Ok(ModuleQueryRun::HeaderParseFailed { diagnostics }) => {
                        header_parse_failed_files.push(HeaderParseFailedFile {
                            file: display,
                            diagnostics,
                        });
                    }
                    Ok(ModuleQueryRun::Unsupported) => {
                        unsupported_files.push(display);
                    }
                    Err(ServerError::Io(_)) => {}
                    Err(err) => return Err(err),
                }
            }

            hits.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then(a.line.cmp(&b.line))
                    .then(a.column.cmp(&b.column))
            });

            let result = ReferencesInProjectResult {
                references: hits,
                truncated,
                files_scanned,
                unsupported_files,
                header_parse_failed_files,
                missing_imports_files,
            };
            Ok(attach_query_notes(
                Response::ok(result, freshness),
                any_freshly_processed,
                &[],
            ))
        })
        .await
}

// --- file_diagnostics --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FileDiagnosticsRequest {
    /// Path to a `.lean` file. Resolved against the resolved project root
    /// if relative.
    pub file: PathBuf,
    #[serde(default)]
    pub project: Option<String>,
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
    Elaborated {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
    },
    /// The file's header did not parse; the body was never elaborated.
    HeaderParseFailed {
        summary: DiagnosticSummary,
        diagnostics: Vec<Diagnostic>,
        truncated: bool,
    },
    Unsupported,
}

/// # Errors
///
/// As [`goal_at_position`].
pub async fn file_diagnostics(
    ctx: &ToolContext,
    req: FileDiagnosticsRequest,
) -> Result<Response<FileDiagnosticsResult>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &[]);
            let outcome = run_module_query(&project, &req.file, LeanWorkerModuleQuery::Diagnostics).await?;
            let (result, fresh, missing) = match outcome {
                ModuleQueryRun::Ready {
                    result: LeanWorkerModuleQueryResult::Diagnostics(diagnostics),
                    freshly_processed,
                    missing_imports,
                } => {
                    let projected = project_failure(&diagnostics);
                    let diagnostics = sort_diagnostics(projected.diagnostics);
                    let summary = DiagnosticSummary::from_diagnostics(&diagnostics);
                    (
                        FileDiagnosticsResult::Elaborated {
                            summary,
                            diagnostics,
                            truncated: projected.truncated,
                        },
                        freshly_processed,
                        missing_imports,
                    )
                }
                ModuleQueryRun::Ready {
                    freshly_processed,
                    missing_imports,
                    ..
                } => (FileDiagnosticsResult::Unsupported, freshly_processed, missing_imports),
                ModuleQueryRun::HeaderParseFailed { diagnostics } => {
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
                ModuleQueryRun::Unsupported => {
                    return Ok(Response::ok(FileDiagnosticsResult::Unsupported, freshness));
                }
            };
            Ok(attach_query_notes(Response::ok(result, freshness), fresh, &missing))
        })
        .await
}

fn sort_diagnostics(mut diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    diagnostics.sort_by_key(|d| d.position.as_ref().map_or((u32::MAX, u32::MAX), |p| (p.line, p.column)));
    diagnostics
}

// --- shared plumbing ---------------------------------------------------

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

async fn run_module_query(
    project: &Arc<LeanProject>,
    path: &Path,
    query: LeanWorkerModuleQuery,
) -> Result<ModuleQueryRun> {
    let resolved = resolve_path(project.canonical_root(), path);
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = hash_bytes(&bytes);
    let key = ModuleQueryKey::from_query(&query);
    if let Some(outcome) = project.module_query_cache().get(&resolved, hash, &key) {
        return Ok(route_query_outcome(outcome, false));
    }

    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    let outcome = process_module_query(project, source, query).await?;
    project
        .module_query_cache()
        .insert(resolved, hash, key, outcome.clone());
    Ok(route_query_outcome(outcome, true))
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

fn attach_query_notes<T>(mut response: Response<T>, freshly_processed: bool, missing_imports: &[String]) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    if freshly_processed {
        response.next_actions.push(
            "module query result cached; repeating the same query against the same file contents reuses it".to_owned(),
        );
    }
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
mod tests {
    use super::header_imports;

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
}
