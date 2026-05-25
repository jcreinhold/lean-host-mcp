//! Tools that drive the Lean session directly: `elaborate`, `kernel_check`,
//! `infer_type`, `whnf`, `is_def_eq`, `hover_by_name`.
//!
//! Every tool returns a [`Response`] envelope; failure modes Lean reports
//! (parse errors, elaboration errors, kernel rejections, meta timeouts) are
//! part of the *successful* tool result—clients branch on the `status`
//! field. Only infrastructure failures (worker dead, runtime not initialised)
//! escape as MCP errors—see [`crate::error::ServerError`].

// Tool handlers receive owned request structs by design (they consume the
// request and forward owned strings into the worker actor); the
// pass-by-value flag is intentional.
#![allow(clippy::needless_pass_by_value)]

use lean_rs_worker::{LeanWorkerDeclarationFilter, LeanWorkerElabOptions, LeanWorkerMetaTransparency};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::envelope::Response;
use crate::error::Result;
use crate::projections::{
    DeclarationRow, ElabFailure, ElabSuccess, KernelOutcome, MetaOutcome, map_worker_err, project_declaration_row,
    project_elab_result, project_kernel_result, project_meta_bool, project_meta_rendered,
};
use crate::tools::{ToolContext, new_session_id};

/// Reducibility view for `is_def_eq`. Mirrors `LeanWorkerMetaTransparency`
/// from the worker layer; kept local so the wire schema doesn't depend on
/// `lean-rs-worker` derives.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Transparency {
    #[default]
    Default,
    Reducible,
    Instances,
    All,
}

impl Transparency {
    fn to_worker(self) -> LeanWorkerMetaTransparency {
        match self {
            Self::Default => LeanWorkerMetaTransparency::Default,
            Self::Reducible => LeanWorkerMetaTransparency::Reducible,
            Self::Instances => LeanWorkerMetaTransparency::Instances,
            Self::All => LeanWorkerMetaTransparency::All,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ElaborateRequest {
    /// Lean source for a single term, e.g. `"(Nat.succ 0 : Nat)"`. Must be
    /// a term, not a command.
    pub source: String,
    /// Module imports to elaborate against. Empty = use server defaults.
    #[serde(default)]
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "PascalCase")]
pub enum ElaborateResult {
    Ok(ElabSuccess),
    Failed(ElabFailure),
}

/// # Errors
///
/// Surfaces `ServerError` only on infrastructure failures (Lean runtime
/// not reachable). Lean-reported elaboration failures travel in the
/// `Failed` variant of the result.
pub async fn elaborate(ctx: &ToolContext, req: ElaborateRequest) -> Result<Response<ElaborateResult>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let imports = ctx.project.effective_imports(&req.imports);
    let source = req.source;
    let outcome = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .elaborate(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_elab_result(result))
        })
        .await?;
    let result = match outcome {
        Ok(success) => ElaborateResult::Ok(success),
        Err(failure) => ElaborateResult::Failed(failure),
    };
    Ok(Response::ok(result, freshness))
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KernelCheckRequest {
    /// Lean declaration source (theorem, definition, etc.) to type-check.
    pub source: String,
    #[serde(default)]
    pub imports: Vec<String>,
}

/// # Errors
///
/// Infrastructure failures only; Lean rejections travel as `KernelOutcome::status`.
pub async fn kernel_check(ctx: &ToolContext, req: KernelCheckRequest) -> Result<Response<KernelOutcome>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let imports = ctx.project.effective_imports(&req.imports);
    let source = req.source;
    let outcome = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .kernel_check(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_kernel_result(result))
        })
        .await?;
    Ok(Response::ok(outcome, freshness))
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct InferTypeRequest {
    /// A term whose type to infer.
    pub term: String,
    #[serde(default)]
    pub imports: Vec<String>,
}

/// # Errors
///
/// Infrastructure failures only; meta-domain failures travel as `MetaOutcome::status`.
pub async fn infer_type(ctx: &ToolContext, req: InferTypeRequest) -> Result<Response<MetaOutcome>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let imports = ctx.project.effective_imports(&req.imports);
    let term = req.term;
    let outcome = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .infer_type(&term, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_rendered(result))
        })
        .await?;
    Ok(attach_render_warning(
        Response::ok(outcome.clone(), freshness),
        &outcome,
    ))
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WhnfRequest {
    /// A term to reduce to weak-head normal form.
    pub term: String,
    #[serde(default)]
    pub imports: Vec<String>,
}

/// # Errors
///
/// Infrastructure failures only.
pub async fn whnf(ctx: &ToolContext, req: WhnfRequest) -> Result<Response<MetaOutcome>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let imports = ctx.project.effective_imports(&req.imports);
    let term = req.term;
    let outcome = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .whnf(&term, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_rendered(result))
        })
        .await?;
    Ok(attach_render_warning(
        Response::ok(outcome.clone(), freshness),
        &outcome,
    ))
}

/// Propagate the worker's `raw_fallback_used` flag into an envelope
/// warning. Keeps the projection layer free of warning vocabulary.
fn attach_render_warning(resp: Response<MetaOutcome>, outcome: &MetaOutcome) -> Response<MetaOutcome> {
    if outcome.raw_fallback_used {
        resp.warn("optional `meta_pp_expr` shim missing on the capability dylib; rendered via `Expr.toString` fallback")
    } else {
        resp
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IsDefEqRequest {
    pub lhs: String,
    pub rhs: String,
    #[serde(default)]
    pub imports: Vec<String>,
    /// Reducibility view to run under. Omit for Lean's standard
    /// (`default`) transparency. Accepted: `default` | `reducible` |
    /// `instances` | `all`.
    #[serde(default)]
    pub transparency: Option<Transparency>,
}

/// # Errors
///
/// Infrastructure failures only.
pub async fn is_def_eq(ctx: &ToolContext, req: IsDefEqRequest) -> Result<Response<MetaOutcome>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let transparency = req.transparency.unwrap_or_default().to_worker();
    let imports = ctx.project.effective_imports(&req.imports);
    let lhs = req.lhs;
    let rhs = req.rhs;
    let outcome = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .is_def_eq(&lhs, &rhs, transparency, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_bool(result))
        })
        .await?;
    Ok(Response::ok(outcome, freshness))
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HoverByNameRequest {
    /// Fully-qualified Lean name, e.g. `Nat.add_zero`.
    pub name: String,
    #[serde(default)]
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HoverByNameResult {
    Found(DeclarationRow),
    Missing { name: String },
}

/// # Errors
///
/// Infrastructure failures only; missing names return `HoverByNameResult::Missing`.
pub async fn hover_by_name(ctx: &ToolContext, req: HoverByNameRequest) -> Result<Response<HoverByNameResult>> {
    let freshness = ctx.freshness(&req.imports, &new_session_id());
    let imports = ctx.project.effective_imports(&req.imports);
    let name = req.name.clone();
    let row = ctx
        .project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let row = session.describe(&name, None, None).map_err(map_worker_err)?;
            Ok(row.map(project_declaration_row))
        })
        .await?;
    let result = match row {
        Some(row) => HoverByNameResult::Found(row),
        None => HoverByNameResult::Missing { name: req.name },
    };
    Ok(Response::ok(result, freshness))
}

// --- index-tool helpers (called from `tools::index::ensure_index`) ------

/// List every declaration in the open environment as a Vec of fully-
/// qualified strings.
///
/// # Errors
///
/// Infrastructure failures only.
pub(crate) async fn list_declarations_strings(
    ctx: &ToolContext,
    filter: LeanWorkerDeclarationFilter,
    imports: Vec<String>,
) -> Result<Vec<String>> {
    ctx.project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            session
                .list_declarations_strings(&filter, None, None)
                .map_err(map_worker_err)
        })
        .await
}

/// Bulk-describe every declaration in `names`. Output order matches input
/// order; entries the worker reports as `kind == "missing"` are omitted.
///
/// # Errors
///
/// Infrastructure failures only.
pub(crate) async fn describe_bulk(
    ctx: &ToolContext,
    names: Vec<String>,
    imports: Vec<String>,
) -> Result<Vec<DeclarationRow>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    ctx.project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let rows = session.describe_bulk(&refs, None, None).map_err(map_worker_err)?;
            Ok(rows
                .into_iter()
                .filter(|r| r.kind != "missing")
                .map(project_declaration_row)
                .collect())
        })
        .await
}

fn elab_opts() -> LeanWorkerElabOptions {
    LeanWorkerElabOptions::new()
}
