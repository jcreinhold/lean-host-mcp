//! Position-based tools: `goal_at_position`, `type_at_position`,
//! `references_of_name`.
//!
//! All three project the same upstream value —
//! [`ProcessedFile`](lean_rs_host::host::process::ProcessedFile) — into a
//! tool-specific result enum. Repeated calls against the same source bytes
//! reuse a cached projection (see [`crate::cache::ProcessedFileCache`]).
//!
//! The Lean shim is optional. When the loaded capability dylib was built
//! before `lean-rs` 0.1.3, [`SessionHost::process_file`](crate::SessionHost::process_file)
//! returns [`ProcessFileOutcome::Unsupported`], which propagates as the
//! `Unsupported` variant of each per-tool result. Tools do not error;
//! callers branch on the `status` tag.

// Tool handlers consume their request structs (owned strings into the
// SessionHost channel); pass-by-value is intentional.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_rs_host::host::process::{NameRefNode, ProcessFileOutcome, ProcessedFile};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::cache::{ProcessedFileCache, hash_bytes};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
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
    #[serde(default)]
    pub imports: Vec<String>,
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
    Unsupported,
}

/// # Errors
///
/// Returns `ServerError::Io` when the file cannot be read, and propagates
/// `ServerError::Lean` from the underlying `process_file` call.
pub async fn goal_at_position(ctx: &ToolContext, req: GoalAtPositionRequest) -> Result<Response<GoalAtPositionResult>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let (file, fresh) = match ensure_processed(ctx, &req.file, req.imports).await? {
        EnsureOutcome::Ready {
            file,
            freshly_processed,
        } => (file, freshly_processed),
        EnsureOutcome::Unsupported => return Ok(Response::ok(GoalAtPositionResult::Unsupported, freshness)),
    };
    let result = match file.tactic_at(req.line, req.column) {
        Some(node) => GoalAtPositionResult::Goal {
            goals_before: node.goals_before.clone(),
            goals_after: node.goals_after.clone(),
            span: SourceSpan {
                start_line: node.start_line,
                start_column: node.start_column,
                end_line: node.end_line,
                end_column: node.end_column,
            },
        },
        None => GoalAtPositionResult::NoTacticContext,
    };
    Ok(attach_processed_hint(Response::ok(result, freshness), fresh))
}

// --- type_at_position --------------------------------------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TypeAtPositionRequest {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    #[serde(default)]
    pub imports: Vec<String>,
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
    Unsupported,
}

/// # Errors
///
/// As [`goal_at_position`].
pub async fn type_at_position(ctx: &ToolContext, req: TypeAtPositionRequest) -> Result<Response<TypeAtPositionResult>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let (file, fresh) = match ensure_processed(ctx, &req.file, req.imports).await? {
        EnsureOutcome::Ready {
            file,
            freshly_processed,
        } => (file, freshly_processed),
        EnsureOutcome::Unsupported => return Ok(Response::ok(TypeAtPositionResult::Unsupported, freshness)),
    };
    let result = match file.term_at(req.line, req.column) {
        Some(node) => TypeAtPositionResult::Term {
            expr: node.expr_str.clone(),
            type_str: node.type_str.clone(),
            expected_type: node.expected_type_str.clone(),
            span: SourceSpan {
                start_line: node.start_line,
                start_column: node.start_column,
                end_line: node.end_line,
                end_column: node.end_column,
            },
        },
        None => TypeAtPositionResult::NoTerm,
    };
    Ok(attach_processed_hint(Response::ok(result, freshness), fresh))
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
    #[serde(default)]
    pub imports: Vec<String>,
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
pub struct ReferencesOfNameResult {
    pub references: Vec<ReferenceHit>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Files that returned `Unsupported` (capability dylib lacks the
    /// `process_with_info_tree` shim). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsupported_files: Vec<String>,
}

/// # Errors
///
/// Returns `ServerError::Lean` if the underlying `process_file` call fails
/// for a non-`Unsupported` reason. Files that cannot be read are skipped
/// silently — same policy as [`crate::tools::scan::project_scan`].
pub async fn references_of_name(
    ctx: &ToolContext,
    req: ReferencesOfNameRequest,
) -> Result<Response<ReferencesOfNameResult>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let root = PathBuf::from(&ctx.lake_root);
    let files = if req.files.is_empty() {
        enumerate_lean_files(&root)
    } else {
        req.files.iter().map(|p| resolve_path(&root, p)).collect()
    };

    let mut hits: Vec<ReferenceHit> = Vec::new();
    let mut unsupported_files: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut any_freshly_processed = false;

    'outer: for path in files {
        let imports = req.imports.clone();
        match ensure_processed(ctx, &path, imports).await? {
            EnsureOutcome::Ready {
                file,
                freshly_processed,
            } => {
                if freshly_processed {
                    any_freshly_processed = true;
                }
                let display = display_path(&root, &path);
                for node in file.references_of(&req.name) {
                    if hits.len() >= MAX_REFERENCES {
                        truncated = true;
                        break 'outer;
                    }
                    hits.push(project_reference(&display, node));
                }
            }
            EnsureOutcome::Unsupported => {
                unsupported_files.push(display_path(&root, &path));
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
    };
    Ok(attach_processed_hint(
        Response::ok(result, freshness),
        any_freshly_processed,
    ))
}

// --- shared plumbing ---------------------------------------------------

/// Outcome of running a file through the cache-or-process flow.
enum EnsureOutcome {
    Ready {
        file: Arc<ProcessedFile>,
        freshly_processed: bool,
    },
    Unsupported,
}

/// Read `path` from disk, hash, hit-or-fill the cache. On miss, dispatch
/// to [`SessionHost::process_file`](crate::SessionHost::process_file) and
/// insert the result.
async fn ensure_processed(ctx: &ToolContext, path: &Path, imports: Vec<String>) -> Result<EnsureOutcome> {
    let resolved = resolve_path(Path::new(&ctx.lake_root), path);
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let hash = hash_bytes(&bytes);
    let cache: &ProcessedFileCache = &ctx.processed_files;
    if let Some(file) = cache.get(&resolved, hash) {
        return Ok(EnsureOutcome::Ready {
            file,
            freshly_processed: false,
        });
    }
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    match ctx.host.process_file(source, imports).await? {
        ProcessFileOutcome::Processed(pf) => {
            let arc = Arc::new(pf);
            cache.insert(resolved, hash, Arc::clone(&arc));
            Ok(EnsureOutcome::Ready {
                file: arc,
                freshly_processed: true,
            })
        }
        ProcessFileOutcome::Unsupported => Ok(EnsureOutcome::Unsupported),
        _ => Err(ServerError::Lean(
            "process_with_info_tree returned an unknown outcome variant".into(),
        )),
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

fn project_reference(file: &str, node: &NameRefNode) -> ReferenceHit {
    ReferenceHit {
        file: file.to_owned(),
        line: node.start_line,
        column: node.start_column,
        end_line: node.end_line,
        end_column: node.end_column,
        kind: if node.is_binder { "def" } else { "ref" },
    }
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

fn attach_processed_hint<T>(resp: Response<T>, freshly_processed: bool) -> Response<T>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    if freshly_processed {
        resp.hint("file processed and cached; subsequent position queries against the same contents reuse the cache")
    } else {
        resp
    }
}
