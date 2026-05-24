//! Tools that drive the Lean session directly: `elaborate`, `kernel_check`,
//! `infer_type`, `whnf`, `is_def_eq`, `hover_by_name`.
//!
//! Every tool returns a [`Response`] envelope; failure modes Lean reports
//! (parse errors, elaboration errors, kernel rejections, meta timeouts) are
//! part of the *successful* tool result — clients branch on the `status`
//! field. Only infrastructure failures (worker dead, runtime not initialised)
//! escape as MCP errors — see [`crate::error::ServerError`].

// Tool handlers receive owned request structs by design (they consume the
// request and forward owned strings into the `SessionHost` channel); the
// pass-by-value flag is intentional.
#![allow(clippy::needless_pass_by_value)]

use lean_rs_worker::LeanWorkerMetaTransparency;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::envelope::Response;
use crate::error::Result;
use crate::session::{DeclarationRow, ElabFailure, ElabSuccess, KernelOutcome, MetaOutcome};
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
    let result = match ctx.host.elaborate(req.source, req.imports).await? {
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
    let outcome = ctx.host.kernel_check(req.source, req.imports).await?;
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
    let outcome = ctx.host.infer_type(req.term, req.imports).await?;
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
    let outcome = ctx.host.whnf(req.term, req.imports).await?;
    Ok(attach_render_warning(
        Response::ok(outcome.clone(), freshness),
        &outcome,
    ))
}

/// Propagate the worker's `raw_fallback_used` flag into an envelope
/// warning. Keeps `session.rs` free of warning vocabulary.
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
    let outcome = ctx.host.is_def_eq(req.lhs, req.rhs, req.imports, transparency).await?;
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
    let result = match ctx.host.describe(req.name.clone(), req.imports).await? {
        Some(row) => HoverByNameResult::Found(row),
        None => HoverByNameResult::Missing { name: req.name },
    };
    Ok(Response::ok(result, freshness))
}
