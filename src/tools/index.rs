//! Tools that read the `SQLite`-backed [`DeclarationIndex`]: `find_symbol`,
//! `find_lemma`, `outline`.
//!
//! Each handler is intentionally thin—shape the request, ensure the
//! index is fresh, call one `DeclarationIndex` method, return. The index
//! itself owns `SQLite`, schema, and storage; the rebuild pipeline
//! (filter → list → bulk-describe → insert) is orchestrated here so the
//! index module stays free of `lean-rs-worker` types.

#![allow(clippy::needless_pass_by_value)]

use std::sync::Arc;

use lean_rs_worker::LeanWorkerDeclarationFilter;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::Result;
use crate::index::{IndexedDeclaration, fingerprint_lake_project};
use crate::project::LeanProject;
use crate::tools::{ToolContext, freshness_for, lean as lean_tools};

/// Default + cap for the `limit` request field—handlers clamp to this
/// range so a missing or oversized value can't return more than 500 rows.
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindSymbolRequest {
    /// Substring to match against declaration names. Case-insensitive.
    pub query: String,
    /// Imports the index was built against. Empty = server defaults.
    /// Drives the rebuild that fires when the Lake manifest has changed.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Maximum rows to return. Defaults to 50, clamped to 500.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional explicit project (absolute path to Lake root). When
    /// omitted, the server resolves via env → cwd-walk → config default.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindLemmaRequest {
    pub query: String,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OutlineRequest {
    /// Optional fully-qualified name prefix, e.g. `"Nat."` for everything
    /// in the `Nat` namespace. Omitted = the full table.
    #[serde(default)]
    pub module_prefix: Option<String>,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
}

/// Case-insensitive substring search across all declaration names.
///
/// # Errors
///
/// Infrastructure failures only—session errors during rebuild, or
/// `SQLite` errors during the search.
pub async fn find_symbol(ctx: &ToolContext, req: FindSymbolRequest) -> Result<Response<Vec<IndexedDeclaration>>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let rebuilt = ensure_index(&project, req.imports).await?;
            let limit = clamp_limit(req.limit);
            let hits = project.index().search(&req.query, limit)?;
            Ok(attach_rebuild_hint(Response::ok(hits, freshness), rebuilt))
        })
        .await
}

/// As [`find_symbol`], restricted to declarations Lean reports as
/// `kind = "theorem"`.
///
/// # Errors
///
/// Infrastructure failures only.
pub async fn find_lemma(ctx: &ToolContext, req: FindLemmaRequest) -> Result<Response<Vec<IndexedDeclaration>>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let rebuilt = ensure_index(&project, req.imports).await?;
            let limit = clamp_limit(req.limit);
            let hits = project.index().search_theorems(&req.query, limit)?;
            Ok(attach_rebuild_hint(Response::ok(hits, freshness), rebuilt))
        })
        .await
}

/// Name-prefix listing. With no prefix, returns the first `limit` rows
/// ordered by name—useful for cold-cache exploration.
///
/// # Errors
///
/// Infrastructure failures only.
pub async fn outline(ctx: &ToolContext, req: OutlineRequest) -> Result<Response<Vec<IndexedDeclaration>>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let rebuilt = ensure_index(&project, req.imports).await?;
            let limit = clamp_limit(req.limit);
            let hits = project.index().outline(req.module_prefix.as_deref(), limit)?;
            Ok(attach_rebuild_hint(Response::ok(hits, freshness), rebuilt))
        })
        .await
}

/// Compare the cached fingerprint to `lake-manifest.json` and rebuild
/// when they diverge. Returns `true` when a rebuild fired, so callers
/// can attach a hint.
async fn ensure_index(project: &Arc<LeanProject>, imports: Vec<String>) -> Result<bool> {
    let fp = fingerprint_lake_project(project.canonical_root())?;
    if project.index().is_fresh(&fp)? {
        return Ok(false);
    }
    let imports = project.effective_imports(&imports);
    let names = lean_tools::list_declarations_strings(project, LeanWorkerDeclarationFilter::default(), imports.clone())
        .await?;
    let rows = lean_tools::describe_bulk(project, names, imports).await?;
    project.index().replace_all(&rows, &fp)?;
    Ok(true)
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn attach_rebuild_hint<T>(resp: Response<T>, rebuilt: bool) -> Response<T>
where
    T: serde::Serialize + schemars::JsonSchema,
{
    if rebuilt {
        resp.hint("declaration index was rebuilt; subsequent queries reuse the cache")
    } else {
        resp
    }
}
