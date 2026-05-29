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
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::Result;
use crate::projections::{DeclarationInspectionResult, project_declaration_inspection};
use crate::tools::source_input::{module_name_for_file, read_query_file};
use crate::tools::{ToolContext, session_imports};

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
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct InspectDeclarationRequest {
    pub name: String,
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub fields: InspectDeclarationFields,
    #[serde(default)]
    pub max_field_bytes: Option<u32>,
    #[serde(default)]
    pub max_total_bytes: Option<u32>,
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
    let budgets = budgets_for(&req);
    let fields = req.fields.into();

    if req.name.trim().is_empty() {
        let runtime = ctx.broker.project_runtime(hint, req.imports.clone()).await?;
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
    let call = ctx
        .broker
        .inspect_declaration(hint, session_imports(imports.clone()), imports, request)
        .await?;
    Ok(Response::ok(project_declaration_inspection(call.value), call.freshness).with_runtime(call.runtime))
}

fn budgets_for(req: &InspectDeclarationRequest) -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: req
            .max_field_bytes
            .unwrap_or(DEFAULT_FIELD_BYTES)
            .clamp(MIN_FIELD_BYTES, MAX_FIELD_BYTES),
        total_bytes: req
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
#[allow(clippy::unwrap_used)]
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
        let low: InspectDeclarationRequest =
            serde_json::from_str(r#"{"name":"Nat.add_zero","max_field_bytes":1,"max_total_bytes":1}"#).unwrap();
        assert_eq!(budgets_for(&low).per_field_bytes, MIN_FIELD_BYTES);
        assert_eq!(budgets_for(&low).total_bytes, MIN_TOTAL_BYTES);

        let high: InspectDeclarationRequest =
            serde_json::from_str(r#"{"name":"Nat.add_zero","max_field_bytes":999999,"max_total_bytes":999999}"#)
                .unwrap();
        assert_eq!(budgets_for(&high).per_field_bytes, MAX_FIELD_BYTES);
        assert_eq!(budgets_for(&high).total_bytes, MAX_TOTAL_BYTES);
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
