//! Declaration inspection for proof work.
//!
//! This is the model-facing declaration surface: inspect one selected
//! declaration by name, or resolve one cursor to a declaration and inspect
//! that name. Search remains owned by `search_for_proof`.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationInspectionFields, LeanWorkerDeclarationInspectionRequest, LeanWorkerDeclarationTargetResult,
    LeanWorkerElabOptions, LeanWorkerModuleCacheStatus, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryBatchResult, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{
    DeclarationInspectionCandidate, DeclarationInspectionResult, map_worker_err, project_declaration_inspection,
};
use crate::tools::{ToolContext, freshness_for, session_imports};

const DECLARATION_TARGET_ID: &str = "declaration_target";
const DEFAULT_FIELD_BYTES: u32 = 8 * 1024;
const MIN_FIELD_BYTES: u32 = 256;
const MAX_FIELD_BYTES: u32 = 64 * 1024;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;
const MIN_TOTAL_BYTES: u32 = 1024;
const MAX_TOTAL_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
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
    /// Fully-qualified Lean declaration name, e.g. `Nat.add_zero`.
    #[serde(default)]
    pub name: Option<String>,
    /// Path to a `.lean` file for cursor-based declaration resolution.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// 1-indexed line for cursor-based declaration resolution.
    #[serde(default)]
    pub line: Option<u32>,
    /// 1-indexed column for cursor-based declaration resolution.
    #[serde(default)]
    pub column: Option<u32>,
    /// Module imports used for name-based inspection. Cursor inspection also
    /// includes imports from the target file header.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Optional explicit project root for this call.
    #[serde(default)]
    pub project: Option<String>,
    /// Optional field selection. Omitted fields default to enabled.
    #[serde(default)]
    pub fields: InspectDeclarationFields,
    /// Per rendered-field byte cap. Defaults to 8192, clamped to 256..65536.
    #[serde(default)]
    pub max_field_bytes: Option<u32>,
    /// Total rendered-text byte cap. Defaults to 65536, clamped to 1024..65536.
    #[serde(default)]
    pub max_total_bytes: Option<u32>,
}

/// Inspect one Lean declaration by name or cursor position.
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
    ctx.broker
        .with_project(hint, move |project| async move {
            let budgets = budgets_for(&req);
            let fields = req.fields.into();

            if let Some(name) = req.name.clone().filter(|name| !name.trim().is_empty()) {
                let result = inspect_name(&project, name, req.imports.clone(), fields, budgets).await?;
                return Ok(Response::ok(result, freshness_for(&project, &req.imports)));
            }

            let Some(file) = req.file.clone() else {
                return Ok(Response::ok(
                    DeclarationInspectionResult::NotFound { name: None },
                    freshness_for(&project, &req.imports),
                )
                .warn("inspect_declaration requires either `name` or `file`/`line`/`column`"));
            };
            let (Some(line), Some(column)) = (req.line, req.column) else {
                return Ok(Response::ok(
                    DeclarationInspectionResult::NotFound { name: None },
                    freshness_for(&project, &req.imports),
                )
                .warn("cursor inspection requires `file`, `line`, and `column`"));
            };

            let cursor = resolve_cursor_target(&project, &file, line, column).await?;
            let mut imports = cursor.imports;
            extend_unique(&mut imports, req.imports);
            let freshness = project.freshness(&imports);

            let mut response = match cursor.result {
                CursorTargetResult::Target { name } => {
                    let result = inspect_name(&project, name, imports, fields, budgets).await?;
                    Response::ok(result, freshness)
                }
                CursorTargetResult::NotFound => {
                    Response::ok(DeclarationInspectionResult::NotFound { name: None }, freshness)
                        .warn("cursor did not resolve to a declaration target")
                }
                CursorTargetResult::Ambiguous { candidates } => {
                    Response::ok(DeclarationInspectionResult::Ambiguous { candidates }, freshness)
                        .warn("cursor resolved to multiple declaration targets")
                }
                CursorTargetResult::Unsupported => Response::ok(DeclarationInspectionResult::Unsupported, freshness),
            };

            if let Some(cache_status) = cursor.cache_status {
                response
                    .next_actions
                    .push(format!("worker module snapshot cache status: {cache_status}"));
            }
            if !cursor.missing_imports.is_empty() {
                response.warnings.push(format!(
                    "file header referenced imports not present in the opened session: {}",
                    cursor.missing_imports.join(", ")
                ));
            }
            Ok(response)
        })
        .await
}

async fn inspect_name(
    project: &crate::project::LeanProject,
    name: String,
    imports: Vec<String>,
    fields: LeanWorkerDeclarationInspectionFields,
    budgets: LeanWorkerOutputBudgets,
) -> Result<DeclarationInspectionResult> {
    let request = LeanWorkerDeclarationInspectionRequest { name, fields, budgets };
    project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(session_imports(imports), None, None)
                .map_err(map_worker_err)?;
            session
                .inspect_declaration(&request, None, None)
                .map(project_declaration_inspection)
                .map_err(map_worker_err)
        })
        .await
}

struct CursorTarget {
    result: CursorTargetResult,
    imports: Vec<String>,
    missing_imports: Vec<String>,
    cache_status: Option<&'static str>,
}

enum CursorTargetResult {
    Target {
        name: String,
    },
    NotFound,
    Ambiguous {
        candidates: Vec<DeclarationInspectionCandidate>,
    },
    Unsupported,
}

async fn resolve_cursor_target(
    project: &crate::project::LeanProject,
    path: &Path,
    line: u32,
    column: u32,
) -> Result<CursorTarget> {
    let input = read_query_file(project.canonical_root(), path)?;
    let mut imports = input.imports.clone();
    if let Some(module) = module_name_for_file(project.canonical_root(), &input.resolved) {
        extend_unique(&mut imports, vec![module]);
    }
    let file_label = input.resolved.to_string_lossy().into_owned();
    let selectors = vec![LeanWorkerModuleQuerySelector::DeclarationTarget {
        id: DECLARATION_TARGET_ID.to_owned(),
        name: None,
        line: Some(line),
        column: Some(column),
    }];
    let budgets = LeanWorkerOutputBudgets {
        per_field_bytes: MIN_FIELD_BYTES,
        total_bytes: MIN_TOTAL_BYTES,
    };
    let outcome = process_target_query(project, input.source, file_label, selectors, budgets).await?;
    Ok(match outcome {
        LeanWorkerModuleQueryBatchOutcome::Ok { result, facts, .. } => CursorTarget {
            result: project_target_result(result.items),
            imports,
            missing_imports: Vec::new(),
            cache_status: Some(cache_status_label(facts.cache_status)),
        },
        LeanWorkerModuleQueryBatchOutcome::MissingImports {
            result, missing, facts, ..
        } => CursorTarget {
            result: project_target_result(result.items),
            imports,
            missing_imports: missing,
            cache_status: Some(cache_status_label(facts.cache_status)),
        },
        LeanWorkerModuleQueryBatchOutcome::HeaderParseFailed { facts, .. } => CursorTarget {
            result: CursorTargetResult::NotFound,
            imports,
            missing_imports: Vec::new(),
            cache_status: Some(cache_status_label(facts.cache_status)),
        },
        LeanWorkerModuleQueryBatchOutcome::Unsupported => CursorTarget {
            result: CursorTargetResult::Unsupported,
            imports,
            missing_imports: Vec::new(),
            cache_status: None,
        },
        _ => CursorTarget {
            result: CursorTargetResult::Unsupported,
            imports,
            missing_imports: Vec::new(),
            cache_status: None,
        },
    })
}

async fn process_target_query(
    project: &crate::project::LeanProject,
    source: String,
    file_label: String,
    selectors: Vec<LeanWorkerModuleQuerySelector>,
    budgets: LeanWorkerOutputBudgets,
) -> Result<LeanWorkerModuleQueryBatchOutcome> {
    let imports = session_imports(header_imports(&source));
    project
        .submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let options = LeanWorkerElabOptions::new().file_label(&file_label);
            session
                .process_module_query_batch(&source, &selectors, &budgets, &options, None, None)
                .map_err(map_worker_err)
        })
        .await
}

fn project_target_result(items: Vec<lean_rs_worker_parent::LeanWorkerModuleQueryBatchItem>) -> CursorTargetResult {
    for item in items {
        let lean_rs_worker_parent::LeanWorkerModuleQueryBatchItem::Ok { id, result } = item else {
            continue;
        };
        if id != DECLARATION_TARGET_ID {
            continue;
        }
        let LeanWorkerModuleQueryBatchResult::DeclarationTarget(target) = *result else {
            continue;
        };
        return match target {
            LeanWorkerDeclarationTargetResult::Target { info } => CursorTargetResult::Target {
                name: info.declaration_name,
            },
            LeanWorkerDeclarationTargetResult::NotFound => CursorTargetResult::NotFound,
            LeanWorkerDeclarationTargetResult::Ambiguous { candidates } => CursorTargetResult::Ambiguous {
                candidates: candidates
                    .into_iter()
                    .map(|info| DeclarationInspectionCandidate {
                        name: info.declaration_name,
                        kind: info.declaration_kind,
                    })
                    .collect(),
            },
            _ => CursorTargetResult::NotFound,
        };
    }
    CursorTargetResult::NotFound
}

struct QueryFile {
    resolved: PathBuf,
    imports: Vec<String>,
    source: String,
}

fn read_query_file(root: &Path, path: &Path) -> Result<QueryFile> {
    let resolved = resolve_path(root, path).canonicalize().map_err(ServerError::Io)?;
    let bytes = std::fs::read(&resolved).map_err(ServerError::Io)?;
    let source = String::from_utf8(bytes).map_err(|e| ServerError::Internal(format!("file not UTF-8: {e}")))?;
    let imports = header_imports(&source);
    Ok(QueryFile {
        resolved,
        imports,
        source,
    })
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn module_name_for_file(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.extension()? != "lean" {
        return None;
    }
    let stemmed = relative.with_extension("");
    let parts = stemmed
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() { None } else { Some(parts.join(".")) }
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

fn cache_status_label(status: LeanWorkerModuleCacheStatus) -> &'static str {
    match status {
        LeanWorkerModuleCacheStatus::Hit => "hit",
        LeanWorkerModuleCacheStatus::Miss => "miss",
        LeanWorkerModuleCacheStatus::Rebuilt => "rebuilt",
        LeanWorkerModuleCacheStatus::Evicted => "evicted",
        _ => "unknown",
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
        assert_eq!(req.name.as_deref(), Some("Nat.add_zero"));
        assert!(req.file.is_none());
        assert!(req.fields.statement);
        assert!(req.fields.docstring);
    }

    #[test]
    fn inspect_request_accepts_cursor_mode() {
        let req: InspectDeclarationRequest = serde_json::from_str(r#"{"file":"A.lean","line":4,"column":2}"#).unwrap();
        assert_eq!(req.file, Some(PathBuf::from("A.lean")));
        assert_eq!(req.line, Some(4));
        assert_eq!(req.column, Some(2));
        assert!(req.name.is_none());
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
}
