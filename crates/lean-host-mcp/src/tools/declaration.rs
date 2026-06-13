//! Declaration inspection for proof work.
//!
//! This is the model-facing declaration surface: inspect one selected
//! declaration by name. Search remains owned by `search_for_proof`.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::PathBuf;

use lean_rs_worker_parent::{
    LeanWorkerDeclarationInspectionFields, LeanWorkerDeclarationInspectionRequest, LeanWorkerOutputBudgets,
    LeanWorkerRendering,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};

use crate::broker::ProjectHint;
use crate::diagnosis::{CallOutcome, IncompleteCause, classify_missing_olean, warn_needs_build};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{DeclarationInspectionResult, project_declaration_inspection};
use crate::tools::source_input::{module_name_for_file, read_query_file};
use crate::tools::{OutputBudgetOverrides, ToolContext, session_imports};

const DEFAULT_FIELD_BYTES: u32 = 8 * 1024;
const MIN_FIELD_BYTES: u32 = 256;
const MAX_FIELD_BYTES: u32 = 64 * 1024;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;
const MIN_TOTAL_BYTES: u32 = 1024;
const MAX_TOTAL_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, JsonSchema)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "field-selection booleans mirror the lean-rs declaration inspection request"
)]
pub struct InspectDeclarationFields {
    #[serde(default = "default_true")]
    pub source: bool,
    #[serde(default = "default_true")]
    pub statement: bool,
    #[serde(default = "default_true")]
    pub docstring: bool,
    #[serde(default = "default_true")]
    pub attributes: bool,
    #[serde(default = "default_true")]
    pub flags: bool,
}

impl<'de> Deserialize<'de> for InspectDeclarationFields {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        if value.is_null() {
            return Ok(Self::default());
        }
        if let Some(items) = value.as_array() {
            let mut fields = Self {
                source: false,
                statement: false,
                docstring: false,
                attributes: false,
                flags: false,
            };
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(serde::de::Error::custom("field selection list entries must be strings"));
                };
                match name {
                    "source" => fields.source = true,
                    "statement" | "type" => fields.statement = true,
                    "docstring" | "docs" => fields.docstring = true,
                    "attributes" => fields.attributes = true,
                    "flags" => fields.flags = true,
                    other => {
                        return Err(serde::de::Error::custom(format!(
                            "unknown declaration inspection field `{other}`"
                        )));
                    }
                }
            }
            return Ok(fields);
        }
        #[derive(Deserialize)]
        #[allow(
            clippy::struct_excessive_bools,
            reason = "helper mirrors declaration inspection field-selection booleans"
        )]
        struct FieldObject {
            #[serde(default = "default_true")]
            source: bool,
            #[serde(default = "default_true")]
            statement: bool,
            #[serde(default = "default_true")]
            docstring: bool,
            #[serde(default = "default_true")]
            attributes: bool,
            #[serde(default = "default_true")]
            flags: bool,
        }
        let fields = FieldObject::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            source: fields.source,
            statement: fields.statement,
            docstring: fields.docstring,
            attributes: fields.attributes,
            flags: fields.flags,
        })
    }
}

impl Default for InspectDeclarationFields {
    fn default() -> Self {
        Self {
            source: true,
            statement: true,
            docstring: true,
            attributes: true,
            flags: true,
        }
    }
}

impl From<InspectDeclarationFields> for LeanWorkerDeclarationInspectionFields {
    fn from(fields: InspectDeclarationFields) -> Self {
        Self {
            source: fields.source,
            statement: fields.statement,
            docstring: fields.docstring,
            attributes: fields.attributes,
            flags: fields.flags,
            // Default to notation-aware pretty-printing (the worker renders with
            // `ppExpr`, `pp.universes false`, and falls back to Raw on failure).
            // The `raw_statement` request flag overrides this in the handler.
            rendering: LeanWorkerRendering::Pretty,
            proof_search: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct InspectDeclarationRequest {
    /// Fully-qualified Lean declaration name.
    pub name: String,
    /// Path to a `.lean` file; relative paths resolve against the project root.
    /// Its imports widen the lookup environment.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Imports for explicit-text mode; ignored when `file` is given.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
    /// Fields to return; omit for all. Pass a subset to shrink the result.
    #[serde(default)]
    pub fields: InspectDeclarationFields,
    /// Return the raw elaborated term instead of the pretty-printed signature.
    /// Rarely needed.
    #[serde(default)]
    pub raw_statement: bool,
}

/// Inspect one Lean declaration by name.
///
/// # Errors
///
/// Returns infrastructure failures only. Missing declarations and unsupported
/// worker shims are normal result statuses.
pub async fn inspect_declaration(
    ctx: &ToolContext,
    req: InspectDeclarationRequest,
) -> Result<Response<DeclarationInspectionResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let budgets = budgets_for(&ctx.config.output);
    let mut fields: LeanWorkerDeclarationInspectionFields = req.fields.into();
    if req.raw_statement {
        fields.rendering = LeanWorkerRendering::Raw;
    }

    if req.name.trim().is_empty() {
        let runtime = ctx.broker.project_identity_without_worker(&hint, req.imports.clone())?;
        return Ok(
            Response::ok(DeclarationInspectionResult::NotFound { name: None }, runtime.freshness)
                .with_runtime(runtime.runtime)
                .warn("inspect_declaration requires `name`"),
        );
    }
    let mut imports = req.imports.clone();
    if let Some(file) = req.file.as_ref() {
        let input = read_query_file(&meta.canonical_root, file)?;
        extend_unique(&mut imports, input.imports);
        if let Some(module) = module_name_for_file(&meta.canonical_root, &input.resolved) {
            extend_unique(&mut imports, vec![module]);
        }
    }
    let request = LeanWorkerDeclarationInspectionRequest {
        name: req.name.clone(),
        fields,
        budgets,
    };
    // A missing-`.olean` in the file's import closure means the name could not
    // be resolved against a complete environment; degrade to the shared
    // needs_build verdict rather than letting the raw error propagate (and
    // rather than a dishonest not_found).
    let call = match classify_missing_olean(
        ctx.broker
            .inspect_declaration(hint.clone(), session_imports(imports.clone()), imports.clone(), request)
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return inspection_needs_build_response(ctx, hint, imports, err),
    };
    let projected = project_declaration_inspection(call.value);
    let bare_name_without_context = req.file.is_none() && req.imports.is_empty();
    let mut response = Response::ok(projected.clone(), call.freshness).with_runtime(call.runtime);
    if bare_name_without_context && matches!(projected, DeclarationInspectionResult::NotFound { .. }) {
        response = response.hint(
            "bare-name inspection only sees the opened import profile; pass `file` or explicit `imports` for Mathlib/project declarations",
        );
    }
    Ok(response)
}

/// Build the degraded inspection verdict when the file's import closure hit an
/// unbuilt `.olean`. Freshness/runtime come from the non-spawning broker
/// identity path, paid only on this rare arm.
fn inspection_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    err: ServerError,
) -> Result<Response<DeclarationInspectionResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, imports)?;
    let response = Response::ok(DeclarationInspectionResult::NeedsBuild, base.freshness).with_runtime(base.runtime);
    Ok(warn_needs_build(
        response,
        &IncompleteCause::MissingOlean(err.to_string()),
    ))
}

fn budgets_for(output: &OutputBudgetOverrides) -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: output
            .max_field_bytes
            .unwrap_or(DEFAULT_FIELD_BYTES)
            .clamp(MIN_FIELD_BYTES, MAX_FIELD_BYTES),
        total_bytes: output
            .max_total_bytes
            .unwrap_or(DEFAULT_TOTAL_BYTES)
            .clamp(MIN_TOTAL_BYTES, MAX_TOTAL_BYTES),
    }
}

fn extend_unique(out: &mut Vec<String>, extra: Vec<String>) {
    for import in extra {
        if !out.iter().any(|existing| existing == &import) {
            out.push(import);
        }
    }
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn inspect_request_accepts_name_mode() {
        let req: InspectDeclarationRequest = serde_json::from_str(r#"{"name":"Nat.add_zero"}"#).unwrap();
        assert_eq!(req.name, "Nat.add_zero");
        assert!(req.file.is_none());
        assert!(req.fields.statement);
        assert!(req.fields.docstring);
    }

    #[test]
    fn budgets_are_clamped() {
        let low = OutputBudgetOverrides {
            max_field_bytes: Some(1),
            max_total_bytes: Some(1),
            heartbeat_limit: None,
        };
        assert_eq!(budgets_for(&low).per_field_bytes, MIN_FIELD_BYTES);
        assert_eq!(budgets_for(&low).total_bytes, MIN_TOTAL_BYTES);

        let high = OutputBudgetOverrides {
            max_field_bytes: Some(999_999),
            max_total_bytes: Some(999_999),
            heartbeat_limit: None,
        };
        assert_eq!(budgets_for(&high).per_field_bytes, MAX_FIELD_BYTES);
        assert_eq!(budgets_for(&high).total_bytes, MAX_TOTAL_BYTES);

        let default = OutputBudgetOverrides::default();
        assert_eq!(budgets_for(&default).per_field_bytes, DEFAULT_FIELD_BYTES);
        assert_eq!(budgets_for(&default).total_bytes, DEFAULT_TOTAL_BYTES);
    }

    #[test]
    fn field_selection_defaults_missing_fields_to_enabled() {
        let req: InspectDeclarationRequest =
            serde_json::from_str(r#"{"name":"Nat.add_zero","fields":{"docstring":false}}"#).unwrap();
        assert!(req.fields.source);
        assert!(req.fields.statement);
        assert!(!req.fields.docstring);
        assert!(req.fields.attributes);
        assert!(req.fields.flags);
    }

    #[test]
    fn needs_build_inspection_status_is_needs_build_not_not_found() {
        // The unbuilt-closure degrade must not masquerade as not_found.
        let value = serde_json::to_value(DeclarationInspectionResult::NeedsBuild).unwrap();
        assert_eq!(value["status"], "needs_build");
    }

    #[test]
    fn field_selection_accepts_string_list_shorthand() {
        let req: InspectDeclarationRequest =
            serde_json::from_str(r#"{"name":"Nat.add_zero","fields":["statement","attributes"]}"#).unwrap();
        assert!(!req.fields.source);
        assert!(req.fields.statement);
        assert!(!req.fields.docstring);
        assert!(req.fields.attributes);
        assert!(!req.fields.flags);
    }
}
