//! Internal low-level Lean session probes used by crate tests and benchmarks.
//!
//! These functions are deliberately not registered in the MCP server. The
//! model-facing API exposes proof-work outcomes instead of term/meta
//! primitives.
//!
//! Every tool returns a [`Response`] envelope; failure modes Lean reports
//! (parse errors, elaboration errors, kernel rejections, meta timeouts) are
//! part of the *successful* tool result—clients branch on the `status`
//! field. Only infrastructure failures (worker dead, runtime not initialised)
//! escape as MCP errors; see [`crate::error::ServerError`].

// Tool handlers receive owned request structs by design (they consume the
// request and forward owned strings into the worker actor); the
// pass-by-value flag is intentional.
#![allow(clippy::needless_pass_by_value)]

use lean_rs_worker_parent::{LeanWorkerElabOptions, LeanWorkerMetaTransparency};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::Result;
use crate::projections::{
    ElabFailure, ElabSuccess, KernelOutcome, MetaOutcome, map_worker_err, project_elab_result, project_kernel_result,
    project_meta_bool, project_meta_rendered,
};
use crate::tools::{ToolContext, freshness_for, session_imports};

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
    /// Module imports to elaborate against. Empty = no caller-supplied imports.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Optional explicit project (absolute path to Lake root). When
    /// omitted, the server resolves the project via
    /// env → cwd-walk → config default.
    #[serde(default)]
    pub project: Option<String>,
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
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let imports = session_imports(req.imports);
            let source = req.source;
            let outcome = project
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
        })
        .await
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KernelCheckRequest {
    /// Lean declaration source (theorem, definition, etc.) to type-check.
    pub source: String,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
}

/// # Errors
///
/// Infrastructure failures only; Lean rejections travel as `KernelOutcome::status`.
pub async fn kernel_check(ctx: &ToolContext, req: KernelCheckRequest) -> Result<Response<KernelOutcome>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let imports = session_imports(req.imports);
            let source = req.source;
            let outcome = project
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
        })
        .await
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct InferTypeRequest {
    /// A term whose type to infer.
    pub term: String,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
}

/// # Errors
///
/// Infrastructure failures only; meta-domain failures travel as `MetaOutcome::status`.
pub async fn infer_type(ctx: &ToolContext, req: InferTypeRequest) -> Result<Response<MetaOutcome>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let imports = session_imports(req.imports);
            let term = req.term;
            let outcome = project
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
        })
        .await
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WhnfRequest {
    /// A term to reduce to weak-head normal form.
    pub term: String,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
}

/// # Errors
///
/// Infrastructure failures only.
pub async fn whnf(ctx: &ToolContext, req: WhnfRequest) -> Result<Response<MetaOutcome>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let imports = session_imports(req.imports);
            let term = req.term;
            let outcome = project
                .submit(move |cap| {
                    let mut session = cap
                        .open_session_with_imports(imports, None, None)
                        .map_err(map_worker_err)?;
                    let result = session.whnf(&term, &elab_opts(), None, None).map_err(map_worker_err)?;
                    Ok(project_meta_rendered(result))
                })
                .await?;
            Ok(attach_render_warning(
                Response::ok(outcome.clone(), freshness),
                &outcome,
            ))
        })
        .await
}

/// Propagate the worker's `raw_fallback_used` flag into an envelope
/// warning. Keeps the projection layer free of warning vocabulary.
fn attach_render_warning(resp: Response<MetaOutcome>, outcome: &MetaOutcome) -> Response<MetaOutcome> {
    if outcome.raw_fallback_used {
        resp.warn(
            "optional `meta_pp_expr` shim missing on the worker host shims; rendered via `Expr.toString` fallback",
        )
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
    #[serde(default)]
    pub project: Option<String>,
}

/// # Errors
///
/// Infrastructure failures only.
pub async fn is_def_eq(ctx: &ToolContext, req: IsDefEqRequest) -> Result<Response<MetaOutcome>> {
    let hint = ProjectHint::from_request(req.project);
    ctx.broker
        .with_project(hint, move |project| async move {
            let freshness = freshness_for(&project, &req.imports);
            let transparency = req.transparency.unwrap_or_default().to_worker();
            let imports = session_imports(req.imports);
            let lhs = req.lhs;
            let rhs = req.rhs;
            let outcome = project
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
        })
        .await
}

fn elab_opts() -> LeanWorkerElabOptions {
    LeanWorkerElabOptions::new()
}
