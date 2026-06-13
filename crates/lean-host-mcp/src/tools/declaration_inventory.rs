//! Declaration inventory for `lean_lookup(kind = "declarations")`.
//!
//! Source files use the edit-fresh worker declaration-outline selector. Module
//! requests fall back to the build-fresh `.ilean` declaration index only when
//! the source file is unavailable.

#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationTargetInfo, LeanWorkerElabOptions, LeanWorkerModuleQueryBatchItem,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryBatchResult, LeanWorkerModuleQuerySelector,
    LeanWorkerModuleSourceSpan, LeanWorkerOutputBudgets,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::ProjectHint;
use crate::diagnosis::{CallOutcome, IncompleteCause, classify_missing_olean, warn_needs_build};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{ElabFailure, project_failure};
use crate::tools::source_input::{read_query_file, resolve_path, source_path_for_module};
use crate::tools::{ToolContext, session_imports};
use crate::trust::{ArtifactTrust, display_path};

const DECLARATION_OUTLINE_ID: &str = "declarations";
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;
const DEFAULT_PER_FIELD_BYTES: u32 = 8 * 1024;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeclarationInventoryRequest {
    pub target: DeclarationInventoryTarget,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeclarationInventoryTarget {
    File { path: PathBuf },
    Module { module: String },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeclarationInventoryResult {
    pub status: String,
    pub declarations: Vec<DeclarationInventoryRow>,
    pub truncated: bool,
    pub source: String,
    pub files_scanned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<ElabFailure>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeclarationInventoryRow {
    pub name: String,
    pub short_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub declaration_span: DeclarationSpan,
    pub name_span: DeclarationSpan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_span: Option<DeclarationSpan>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeclarationSpan {
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// List declarations for one file or module.
///
/// # Errors
///
/// Returns infrastructure failures only. Missing sources/build artifacts are
/// represented as structured result statuses.
pub async fn declaration_inventory(
    ctx: &ToolContext,
    req: DeclarationInventoryRequest,
) -> Result<Response<DeclarationInventoryResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    match req.target {
        DeclarationInventoryTarget::File { path } => {
            let resolved = resolve_path(&meta.canonical_root, &path);
            if !resolved.is_file() {
                return metadata_response(
                    ctx,
                    hint,
                    not_found_result(format!(
                        "source file `{}` does not exist",
                        display_path(&meta.canonical_root, &resolved)
                    )),
                );
            }
            worker_declarations(ctx, hint, &meta.canonical_root, &path, limit).await
        }
        DeclarationInventoryTarget::Module { module } => {
            let path = source_path_for_module(&meta.canonical_root, &module);
            if path.is_file() {
                worker_declarations(ctx, hint, &meta.canonical_root, &path, limit).await
            } else {
                index_declarations(ctx, hint, &meta.canonical_root, &module, limit)
            }
        }
    }
}

async fn worker_declarations(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    path: &Path,
    limit: usize,
) -> Result<Response<DeclarationInventoryResult>> {
    let input = read_query_file(root, path)?;
    let source_fact = ArtifactTrust::source_file_edit_fresh(root, &input.resolved);
    let file_label = input.resolved.to_string_lossy().into_owned();
    let selectors = vec![LeanWorkerModuleQuerySelector::DeclarationOutline {
        id: DECLARATION_OUTLINE_ID.to_owned(),
    }];
    let call = match classify_missing_olean(
        ctx.broker
            .process_cached_module_query_batch(
                hint.clone(),
                input.resolved,
                input.hash,
                session_imports(input.imports.clone()),
                input.imports,
                input.source,
                selectors,
                outline_budgets(),
                LeanWorkerElabOptions::new().file_label(&file_label),
            )
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return worker_needs_build_response(ctx, hint, err),
    };
    let mut response = Response::ok(project_worker_outline(call.value, limit), call.freshness)
        .with_runtime(call.runtime)
        .with_trust_artifact(source_fact);
    if matches!(response.result_ref().map(|r| r.status.as_str()), Some("needs_build")) {
        response = warn_needs_build(response, &IncompleteCause::MissingImports(Vec::new()));
    }
    Ok(response)
}

fn index_declarations(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    module: &str,
    limit: usize,
) -> Result<Response<DeclarationInventoryResult>> {
    let index = crate::ilean::declarations_in_module(root, module);
    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    let mut response = match index.status {
        crate::ilean::ModuleDeclarationIndexStatus::ProjectNotBuilt => Response::ok(
            missing_build_result(format!(
                "project build tree is absent; no .ilean declaration index is available for module `{module}`"
            )),
            base.freshness,
        )
        .with_runtime(base.runtime)
        .with_trust_artifact(ArtifactTrust::ilean_project_missing_build()),
        crate::ilean::ModuleDeclarationIndexStatus::ModuleNotBuilt => Response::ok(
            missing_build_result(format!(
                "module `{module}` has no source file and no built .ilean declaration index"
            )),
            base.freshness,
        )
        .with_runtime(base.runtime)
        .with_trust_artifact(ArtifactTrust::ilean_module_missing_build(module)),
        crate::ilean::ModuleDeclarationIndexStatus::Present => {
            let (declarations, truncated) = truncate(index_declarations_rows(&index), limit);
            let fact = if index.stale {
                ArtifactTrust::ilean_module_stale_build(index.module.clone(), display_path(root, &index.index))
            } else {
                ArtifactTrust::ilean_module_build_fresh(index.module.clone(), display_path(root, &index.index))
            };
            Response::ok(
                DeclarationInventoryResult {
                    status: "ok".to_owned(),
                    declarations,
                    truncated,
                    source: "ilean".to_owned(),
                    files_scanned: 1,
                    message: None,
                    diagnostics: None,
                },
                base.freshness,
            )
            .with_runtime(base.runtime)
            .with_trust_artifact(fact)
        }
    };
    if matches!(
        index.status,
        crate::ilean::ModuleDeclarationIndexStatus::ProjectNotBuilt
    ) {
        response = response
            .warn("project build tree is absent; run `lake build` to produce .ilean declaration indices")
            .hint("lake build # produce .ilean declaration indices, then retry");
    }
    Ok(response)
}

fn worker_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    err: ServerError,
) -> Result<Response<DeclarationInventoryResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    let response = Response::ok(missing_build_result(err.to_string()), base.freshness).with_runtime(base.runtime);
    Ok(warn_needs_build(
        response,
        &IncompleteCause::MissingOlean(err.to_string()),
    ))
}

fn metadata_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    result: DeclarationInventoryResult,
) -> Result<Response<DeclarationInventoryResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    Ok(Response::ok(result, base.freshness).with_runtime(base.runtime))
}

fn project_worker_outline(outcome: LeanWorkerModuleQueryBatchOutcome, limit: usize) -> DeclarationInventoryResult {
    match outcome {
        LeanWorkerModuleQueryBatchOutcome::Ok { result, .. } => {
            outline_from_batch(result.items, result.total_truncated, limit)
        }
        LeanWorkerModuleQueryBatchOutcome::MissingImports { result, .. } => {
            let mut out = outline_from_batch(result.items, result.total_truncated, limit);
            "needs_build".clone_into(&mut out.status);
            if out.message.is_none() {
                out.message = Some("declaration outline ran against an incomplete import environment".to_owned());
            }
            out
        }
        LeanWorkerModuleQueryBatchOutcome::HeaderParseFailed { diagnostics, .. } => DeclarationInventoryResult {
            status: "header_parse_failed".to_owned(),
            declarations: Vec::new(),
            truncated: false,
            source: "worker".to_owned(),
            files_scanned: 1,
            message: Some("source header could not be parsed".to_owned()),
            diagnostics: Some(project_failure(&diagnostics)),
        },
        LeanWorkerModuleQueryBatchOutcome::Unsupported => unsupported_result(),
        _ => unsupported_result(),
    }
}

fn outline_from_batch(
    items: Vec<LeanWorkerModuleQueryBatchItem>,
    batch_truncated: bool,
    limit: usize,
) -> DeclarationInventoryResult {
    let Some(item) = items.into_iter().find(|item| item_id(item) == DECLARATION_OUTLINE_ID) else {
        return unsupported_result();
    };
    match item {
        LeanWorkerModuleQueryBatchItem::Ok { result, .. } => match *result {
            LeanWorkerModuleQueryBatchResult::DeclarationOutline(outline) => {
                let (declarations, host_truncated) = truncate(
                    outline.declarations.into_iter().map(worker_declaration_row).collect(),
                    limit,
                );
                DeclarationInventoryResult {
                    status: "ok".to_owned(),
                    declarations,
                    truncated: batch_truncated || outline.truncated || host_truncated,
                    source: "worker".to_owned(),
                    files_scanned: 1,
                    message: None,
                    diagnostics: None,
                }
            }
            LeanWorkerModuleQueryBatchResult::Diagnostics(_)
            | LeanWorkerModuleQueryBatchResult::ProofState(_)
            | LeanWorkerModuleQueryBatchResult::TypeAt(_)
            | LeanWorkerModuleQueryBatchResult::References(_)
            | LeanWorkerModuleQueryBatchResult::DeclarationTarget(_)
            | LeanWorkerModuleQueryBatchResult::SurroundingDeclaration(_) => unsupported_result(),
            _ => unsupported_result(),
        },
        LeanWorkerModuleQueryBatchItem::Unavailable { message, .. } => DeclarationInventoryResult {
            status: "unsupported".to_owned(),
            declarations: Vec::new(),
            truncated: false,
            source: "worker".to_owned(),
            files_scanned: 1,
            message: Some(message),
            diagnostics: None,
        },
        LeanWorkerModuleQueryBatchItem::BudgetExceeded { message, .. } => DeclarationInventoryResult {
            status: "ok".to_owned(),
            declarations: Vec::new(),
            truncated: true,
            source: "worker".to_owned(),
            files_scanned: 1,
            message: Some(message),
            diagnostics: None,
        },
        _ => unsupported_result(),
    }
}

fn item_id(item: &LeanWorkerModuleQueryBatchItem) -> &str {
    match item {
        LeanWorkerModuleQueryBatchItem::Ok { id, .. }
        | LeanWorkerModuleQueryBatchItem::Unavailable { id, .. }
        | LeanWorkerModuleQueryBatchItem::BudgetExceeded { id, .. } => id,
        _ => "",
    }
}

fn worker_declaration_row(info: LeanWorkerDeclarationTargetInfo) -> DeclarationInventoryRow {
    DeclarationInventoryRow {
        name: info.declaration_name,
        short_name: info.short_name,
        kind: Some(info.declaration_kind),
        declaration_span: worker_span(info.declaration_span),
        name_span: worker_span(info.name_span),
        body_span: Some(worker_span(info.body_span)),
    }
}

fn index_declarations_rows(index: &crate::ilean::ModuleDeclarationIndex) -> Vec<DeclarationInventoryRow> {
    index
        .declarations
        .iter()
        .map(|declaration| DeclarationInventoryRow {
            name: declaration.name.clone(),
            short_name: short_name(&declaration.name),
            kind: None,
            declaration_span: index_span(&declaration.declaration_span),
            name_span: index_span(&declaration.selection_span),
            body_span: None,
        })
        .collect()
}

fn truncate(mut declarations: Vec<DeclarationInventoryRow>, limit: usize) -> (Vec<DeclarationInventoryRow>, bool) {
    let truncated = declarations.len() > limit;
    declarations.truncate(limit);
    (declarations, truncated)
}

fn worker_span(span: LeanWorkerModuleSourceSpan) -> DeclarationSpan {
    DeclarationSpan {
        start_line: span.start_line,
        start_column: span.start_column,
        end_line: span.end_line,
        end_column: span.end_column,
    }
}

fn index_span(span: &crate::ilean::DeclSpan) -> DeclarationSpan {
    DeclarationSpan {
        start_line: span.start_line.saturating_add(1),
        start_column: span.start_column.saturating_add(1),
        end_line: span.end_line.saturating_add(1),
        end_column: span.end_column.saturating_add(1),
    }
}

fn short_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_owned()
}

fn not_found_result(message: String) -> DeclarationInventoryResult {
    DeclarationInventoryResult {
        status: "not_found".to_owned(),
        declarations: Vec::new(),
        truncated: false,
        source: "none".to_owned(),
        files_scanned: 0,
        message: Some(message),
        diagnostics: None,
    }
}

fn missing_build_result(message: String) -> DeclarationInventoryResult {
    DeclarationInventoryResult {
        status: "missing_build".to_owned(),
        declarations: Vec::new(),
        truncated: false,
        source: "none".to_owned(),
        files_scanned: 0,
        message: Some(message),
        diagnostics: None,
    }
}

fn unsupported_result() -> DeclarationInventoryResult {
    DeclarationInventoryResult {
        status: "unsupported".to_owned(),
        declarations: Vec::new(),
        truncated: false,
        source: "worker".to_owned(),
        files_scanned: 1,
        message: Some("declaration outline selector is unavailable".to_owned()),
        diagnostics: None,
    }
}

const fn outline_budgets() -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: DEFAULT_PER_FIELD_BYTES,
        total_bytes: DEFAULT_TOTAL_BYTES,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::broker::{BrokerConfig, ProjectBroker};
    use crate::tools::{ToolConfig, ToolContext};
    use crate::trust::{ArtifactKind, TrustScope, TrustStatus};

    fn make_lake_dir(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("inventory");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lakefile.lean"), "package inventory\nlean_lib Demo\n").unwrap();
        std::fs::write(dir.join("lean-toolchain"), "leanprover/lean4:v4.31.0-rc2\n").unwrap();
        std::fs::write(dir.join("lake-manifest.json"), "{}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn test_context(root: std::path::PathBuf) -> (ToolContext, std::sync::Arc<ProjectBroker>) {
        let broker = ProjectBroker::new(BrokerConfig {
            config_default: None,
            env_default: Some(root.clone()),
            cwd: root,
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: std::time::Duration::ZERO,
            semantic_permits: BrokerConfig::default_semantic_permits(),
            semantic_waiters: BrokerConfig::default_semantic_waiters(),
            semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
            semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
        });
        (
            ToolContext {
                broker: std::sync::Arc::clone(&broker),
                config: ToolConfig::default(),
            },
            broker,
        )
    }

    fn copy_ilean(root: &std::path::Path, module: &str, fixture: &str) -> std::path::PathBuf {
        let relative: std::path::PathBuf = module.split('.').collect();
        let index = root.join(".lake/build/lib/lean").join(relative).with_extension("ilean");
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        std::fs::copy(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/ilean")
                .join(fixture),
            &index,
        )
        .unwrap();
        index
    }

    fn has_fact(
        response: &Response<DeclarationInventoryResult>,
        artifact: ArtifactKind,
        scope: TrustScope,
        status: TrustStatus,
    ) -> bool {
        response
            .trust_artifacts
            .iter()
            .any(|fact| fact.artifact == artifact && fact.scope == scope && fact.status == status)
    }

    #[test]
    fn lean_lookup_declarations_limit_truncates_deterministically() {
        let rows = vec![
            DeclarationInventoryRow {
                name: "A.one".to_owned(),
                short_name: "one".to_owned(),
                kind: None,
                declaration_span: DeclarationSpan {
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 4,
                },
                name_span: DeclarationSpan {
                    start_line: 1,
                    start_column: 1,
                    end_line: 1,
                    end_column: 4,
                },
                body_span: None,
            },
            DeclarationInventoryRow {
                name: "A.two".to_owned(),
                short_name: "two".to_owned(),
                kind: None,
                declaration_span: DeclarationSpan {
                    start_line: 2,
                    start_column: 1,
                    end_line: 2,
                    end_column: 4,
                },
                name_span: DeclarationSpan {
                    start_line: 2,
                    start_column: 1,
                    end_line: 2,
                    end_column: 4,
                },
                body_span: None,
            },
        ];
        let (rows, truncated) = truncate(rows, 1);
        assert!(truncated);
        assert_eq!(rows[0].name, "A.one");
    }

    #[test]
    fn lean_lookup_declarations_index_fallback_reports_build_fresh_fact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        copy_ilean(&root, "Demo.A", "demo_a.ilean");
        let (ctx, broker) = test_context(root.clone());

        let response = index_declarations(&ctx, ProjectHint::from_request(None), &root, "Demo.A", 200).unwrap();

        assert_eq!(response.result_ref().unwrap().status, "ok");
        assert_eq!(response.result_ref().unwrap().source, "ilean");
        assert_eq!(response.result_ref().unwrap().declarations[0].name, "Demo.A.foo");
        assert!(has_fact(
            &response,
            ArtifactKind::Ilean,
            TrustScope::Module,
            TrustStatus::BuildFresh
        ));
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn lean_lookup_declarations_index_fallback_reports_stale_fact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        copy_ilean(&root, "Demo.A", "demo_a.ilean");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(root.join("Demo")).unwrap();
        std::fs::write(root.join("Demo/A.lean"), "-- edited after build\n").unwrap();
        let (ctx, broker) = test_context(root.clone());

        let response = index_declarations(&ctx, ProjectHint::from_request(None), &root, "Demo.A", 200).unwrap();

        assert!(has_fact(
            &response,
            ArtifactKind::Ilean,
            TrustScope::Module,
            TrustStatus::StaleBuild
        ));
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn lean_lookup_declarations_missing_build_reports_project_fact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        let (ctx, broker) = test_context(root.clone());

        let response = index_declarations(&ctx, ProjectHint::from_request(None), &root, "Demo.A", 200).unwrap();

        assert_eq!(response.result_ref().unwrap().status, "missing_build");
        assert!(has_fact(
            &response,
            ArtifactKind::Ilean,
            TrustScope::Project,
            TrustStatus::MissingBuild
        ));
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }
}
