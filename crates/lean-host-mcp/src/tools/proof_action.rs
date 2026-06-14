//! Non-mutating proof action tools.
//!
//! `try_proof_step` and `verify_declaration` read a Lean file, send its
//! contents to the worker as an in-memory overlay, and return structured
//! proof/verification outcomes. They never write source files.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationVerificationBatchItem, LeanWorkerDeclarationVerificationBatchRequest,
    LeanWorkerDeclarationVerificationBatchResult, LeanWorkerDeclarationVerificationBatchRow,
    LeanWorkerDeclarationVerificationRequest,
    LeanWorkerDeclarationVerificationResult as WorkerDeclarationVerificationResult,
    LeanWorkerDeclarationVerificationTarget, LeanWorkerElabOptions, LeanWorkerOutputBudgets,
    LeanWorkerProofAttemptRequest, LeanWorkerProofCandidate, LeanWorkerProofEditTarget, LeanWorkerSorryPolicy,
};
use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::broker::ProjectHint;
use crate::diagnosis::{
    CallOutcome, IncompleteCause, NEEDS_BUILD_STATUS, WORKER_RECYCLED_STATUS, classify_missing_olean, execution_taint,
    warn_execution_taint, warn_needs_build,
};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{
    DeclarationVerificationFacts, DeclarationVerificationResult, ElabFailure, ProofAttemptEnvelope, ProofAttemptResult,
    project_declaration_verification, project_proof_attempt,
};
use crate::tools::changed_coverage::{
    ChangedCoverageReport, ChangedCoverageRequest, ChangedCoverageResult, ChangedDeclaration, compute_changed_coverage,
};
use crate::tools::position::{ProofPositionSelector, worker_proof_position};
use crate::tools::source_input::{read_query_file, source_path_for_module};
use crate::tools::{OutputBudgetOverrides, ToolContext, session_imports};
use crate::trust::ArtifactTrust;

const MAX_CANDIDATES: usize = 16;
const MAX_VERIFY_TARGETS: usize = 1000;
const DEFAULT_FIELD_BYTES: u32 = 4 * 1024;
const MIN_FIELD_BYTES: u32 = 256;
const MAX_FIELD_BYTES: u32 = 64 * 1024;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;
const MIN_TOTAL_BYTES: u32 = 1024;
const MAX_TOTAL_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TryProofStepRequest {
    /// Path to a `.lean` file; relative paths resolve against the project root.
    pub file: PathBuf,
    /// Declaration to target within `file`.
    pub declaration: String,
    /// Where in the proof to act; defaults to the pristine entry goal (the
    /// snippet is spliced before the first tactic). See [`ProofPositionSelector`].
    #[serde(default)]
    pub proof_position: ProofPositionSelector,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
    /// Proof text to attempt at the position. Use `snippets` to try several.
    #[serde(default)]
    pub snippet: Option<String>,
    /// Proof snippets to attempt independently at the position, in one call.
    #[serde(default)]
    pub snippets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct VerifyDeclarationRequest {
    /// Path to a `.lean` file; relative paths resolve against the project root.
    pub file: PathBuf,
    /// Declaration to verify within `file`.
    pub declaration: String,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
    /// Treat `sorry`/`admit` as success instead of failure.
    #[serde(default)]
    pub allow_sorry: bool,
    /// Include the axioms the proof depends on (slower).
    #[serde(default)]
    pub report_axioms: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LeanVerifyRequest {
    /// Explicit, file-wide, or module-wide declaration groups to verify.
    pub targets: Vec<LeanVerifyTargetGroup>,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
    /// Treat `sorry`/`admit` as success instead of failure.
    #[serde(default)]
    pub allow_sorry: bool,
    /// Include the axioms each proof depends on (slower).
    #[serde(default)]
    pub report_axioms: bool,
}

#[derive(Debug, Clone)]
pub struct LeanVerifyToolRequest(Value);

impl LeanVerifyToolRequest {
    pub fn into_inner(self) -> Value {
        self.0
    }
}

impl<'de> Deserialize<'de> for LeanVerifyToolRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self)
    }
}

impl JsonSchema for LeanVerifyToolRequest {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LeanVerifyRequest")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        LeanVerifyRequest::json_schema(generator)
    }
}

impl From<VerifyDeclarationRequest> for LeanVerifyRequest {
    fn from(req: VerifyDeclarationRequest) -> Self {
        Self {
            targets: vec![LeanVerifyTargetGroup::Explicit {
                file: req.file,
                declarations: vec![req.declaration],
            }],
            project: req.project,
            allow_sorry: req.allow_sorry,
            report_axioms: req.report_axioms,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeanVerifyTargetGroup {
    Explicit {
        file: PathBuf,
        declarations: Vec<String>,
    },
    FileAll {
        file: PathBuf,
    },
    ModuleAll {
        module: String,
    },
    Changed {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        files: Vec<PathBuf>,
        #[serde(default)]
        include_untracked: bool,
    },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeanVerifyResult {
    pub summary: LeanVerifySummary,
    pub results: Vec<LeanVerifyRow>,
    #[serde(skip_serializing_if = "ChangedCoverageReport::is_empty")]
    pub coverage: ChangedCoverageReport,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeanVerifySummary {
    pub requested: usize,
    pub verified: usize,
    pub failed: usize,
    pub needs_build: usize,
    pub unknown_coverage: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeanVerifyRow {
    pub id: String,
    pub file: String,
    pub declaration: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub verification_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<Box<DeclarationVerificationFacts>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_imports: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<ElabFailure>,
}

/// Try one or more proof snippets against an in-memory source overlay.
///
/// # Errors
///
/// Returns infrastructure failures only. Failed proof candidates and
/// unsupported worker shims are normal result statuses.
pub async fn try_proof_step(ctx: &ToolContext, req: TryProofStepRequest) -> Result<Response<ProofAttemptResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let input = read_query_file(&meta.canonical_root, &req.file)?;
    let source_fact = ArtifactTrust::source_file_edit_fresh(&meta.canonical_root, &input.resolved);
    let file_label = input.resolved.to_string_lossy().into_owned();
    let budgets = proof_action_budgets(&ctx.config.output);
    let candidates = proof_candidates(&req);
    let requested_count = candidates.len();

    if candidates.is_empty() {
        let runtime = ctx
            .broker
            .project_identity_without_worker(&hint, input.imports.clone())?;
        return Ok(Response::ok(
            ProofAttemptResult::Ok {
                result: empty_proof_attempt_envelope(),
                imports: input.imports,
            },
            runtime.freshness,
        )
        .with_runtime(runtime.runtime)
        .with_trust_artifact(source_fact)
        .warn("try_proof_step requires `snippet` or `snippets`"));
    }

    let imports = input.imports;
    let request = LeanWorkerProofAttemptRequest {
        source: input.source,
        edit: LeanWorkerProofEditTarget::Declaration {
            name: req.declaration.clone(),
            position: worker_proof_position(req.proof_position.clone()),
        },
        candidates,
        budgets,
    };
    // A missing-`.olean` in the target's own import closure means the worker
    // could not assemble the environment to attempt anything; degrade to the
    // shared needs_build verdict instead of letting the raw error propagate.
    let call = match classify_missing_olean(
        ctx.broker
            .attempt_proof(
                hint.clone(),
                session_imports(imports.clone()),
                imports.clone(),
                request,
                elab_options(&file_label, ctx.config.output.heartbeat_limit),
            )
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return proof_step_needs_build_response(ctx, hint, imports, source_fact, err),
    };
    let taint = execution_taint(&call.runtime).cloned();
    let mut response = Response::ok(
        refresh_proof_attempt_summary(project_proof_attempt(call.value), requested_count),
        call.freshness,
    )
    .with_runtime(call.runtime);
    response.trust_artifacts.push(source_fact);
    response
        .next_actions
        .push("source file was not modified; apply the chosen snippet manually if desired".to_owned());
    // If the attempt ran against imports the worker could not load, the
    // candidate diagnostics describe a degraded environment; tell the agent.
    let missing = match response.result_ref() {
        Some(ProofAttemptResult::MissingImports { missing, .. }) => Some(missing.clone()),
        _ => None,
    };
    if let Some(missing) = missing {
        response = warn_needs_build(response, &IncompleteCause::MissingImports(missing));
    }
    // A recycle mid-attempt can turn a closing tactic into a spurious `failed`;
    // there is no single verdict to relabel, so flag the whole attempt.
    if let Some(event) = &taint {
        response = warn_execution_taint(response, event);
    }
    warn_partial_proof_attempt(&mut response);
    // A from-scratch tactic block submitted to an explicit `index` / `after_text`
    // position can fail because the binders it re-introduces are already in scope
    // from earlier tactics. The default targets the pristine entry goal, so this
    // never bites there; for explicit positions, point the agent back at the
    // entry/default or at continuing from this position's `goals_after`.
    if attempt_reintroduces_bound_binders(&response) {
        response.warnings.push(
            "a candidate failed to introduce binders that are already in scope at this position; \
             a from-scratch tactic block belongs at the pristine entry goal, not after earlier tactics have run"
                .to_owned(),
        );
        response.next_actions.push(
            "omit `proof_position` (or use the default) to start from the pristine entry goal, \
             or continue this candidate from the position's `goals_after`"
                .to_owned(),
        );
    }
    Ok(response)
}

/// Lean diagnostics for a tactic that tried to introduce binders no longer
/// available — the signature of a from-scratch block run *after* earlier
/// tactics already introduced those binders, rather than at the entry goal.
fn diagnostics_reintroduce_binders(diagnostics: &ElabFailure) -> bool {
    diagnostics.diagnostics.iter().any(|diagnostic| {
        let message = &diagnostic.message;
        // `introN` is the binder-introduction primitive `intro` lowers to; the
        // phrasings vary across Lean versions ("no additional binders or `let`
        // bindings in the goal to introduce", "insufficient number of binders").
        message.contains("introN")
            || message.contains("no additional binders")
            || message.contains("no binders to introduce")
            || message.contains("insufficient number of binders")
    })
}

/// True when some failed candidate carries a binder-reintroduction diagnostic.
/// Read-only over the built response, so it never disturbs the result payload.
fn attempt_reintroduces_bound_binders(response: &Response<ProofAttemptResult>) -> bool {
    let Some(ProofAttemptResult::Ok { result, .. } | ProofAttemptResult::MissingImports { result, .. }) =
        response.result_ref()
    else {
        return false;
    };
    result
        .candidates
        .iter()
        .any(|candidate| candidate.status == "failed" && diagnostics_reintroduce_binders(&candidate.diagnostics))
}

/// Build the degraded envelope when `try_proof_step`'s target import closure
/// hit an unbuilt `.olean`: no candidate could run against an incomplete
/// environment. Mirrors the verify degrade — a `missing_imports` result plus
/// the canonical `needs_build` warning naming the blocking olean.
fn proof_step_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    source_fact: ArtifactTrust,
    err: ServerError,
) -> Result<Response<ProofAttemptResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, imports.clone())?;
    let mut response = Response::ok(needs_build_attempt_result(imports), base.freshness)
        .with_runtime(base.runtime)
        .with_trust_artifact(source_fact);
    response
        .next_actions
        .push("source file was not modified; apply the chosen snippet manually if desired".to_owned());
    Ok(warn_needs_build(
        response,
        &IncompleteCause::MissingOlean(err.to_string()),
    ))
}

/// The proof-attempt result for an unbuilt-dependency degrade: an empty
/// `missing_imports` envelope (nothing ran). Pure, for unit testing.
fn needs_build_attempt_result(imports: Vec<String>) -> ProofAttemptResult {
    ProofAttemptResult::MissingImports {
        result: empty_proof_attempt_envelope(),
        imports,
        missing: Vec::new(),
    }
}

/// Verify one declaration in an in-memory source snapshot.
///
/// # Errors
///
/// Returns infrastructure failures only. Policy failures, missing
/// declarations, and unsupported worker shims are normal result statuses.
pub async fn verify_declaration(
    ctx: &ToolContext,
    req: VerifyDeclarationRequest,
) -> Result<Response<DeclarationVerificationResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let input = read_query_file(&meta.canonical_root, &req.file)?;
    let source_fact = ArtifactTrust::source_file_edit_fresh(&meta.canonical_root, &input.resolved);
    let file_label = input.resolved.to_string_lossy().into_owned();
    let budgets = proof_action_budgets(&ctx.config.output);
    if req.declaration.trim().is_empty() {
        let runtime = ctx
            .broker
            .project_identity_without_worker(&hint, input.imports.clone())?;
        return Ok(
            Response::ok(DeclarationVerificationResult::Unsupported, runtime.freshness)
                .with_runtime(runtime.runtime)
                .with_trust_artifact(source_fact)
                .warn("verify_declaration requires `declaration`"),
        );
    }
    let target = LeanWorkerDeclarationVerificationTarget::Name {
        name: req.declaration.clone(),
    };

    let request = LeanWorkerDeclarationVerificationRequest {
        source: input.source,
        target,
        sorry_policy: if req.allow_sorry {
            LeanWorkerSorryPolicy::Allow
        } else {
            LeanWorkerSorryPolicy::Deny
        },
        report_axioms: req.report_axioms,
        budgets,
    };
    let imports = input.imports;
    // A missing-`.olean` in the target's own import closure means the worker
    // could not assemble the environment to check anything; degrade to the
    // shared needs_build verdict instead of letting the raw error propagate.
    let call = match classify_missing_olean(
        ctx.broker
            .verify_declaration(
                hint.clone(),
                session_imports(imports.clone()),
                imports.clone(),
                request,
                elab_options(&file_label, ctx.config.output.heartbeat_limit),
            )
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return verification_needs_build_response(ctx, hint, imports, source_fact, err),
    };
    // If the worker was recycled/crashed mid-call, a non-positive verdict is a
    // likely casualty of the recycle, not a real result; relabel it honestly
    // before it reaches the agent (verification is monotone, so a `verified`
    // verdict is left trustworthy even under duress).
    let taint = execution_taint(&call.runtime).cloned();
    let mut result = project_declaration_verification(call.value);
    let recycled = taint.is_some() && relabel_recycled_verdict(&mut result);
    if recycled && let Some(event) = taint.as_ref() {
        tracing::debug!(
            cause = %event.cause,
            "relabeled verification verdict to worker_recycled (execution taint)"
        );
    }
    let mut response = Response::ok(result, call.freshness).with_runtime(call.runtime);
    response.trust_artifacts.push(source_fact);
    response
        .next_actions
        .push("source file was not modified by verification".to_owned());
    // Honest diagnostics: route the verdict's resolution health (needs_build
    // vs genuine ambiguity) through the shared renderer, and flag when the
    // axiom walk could not run.
    let (cause, candidates, axiom_warning) = match response.result_ref() {
        Some(result) => (
            verification_incomplete_cause(result),
            verification_ambiguous_candidates(result),
            axiom_unavailable_warning(result, req.report_axioms),
        ),
        None => (None, Vec::new(), None),
    };
    if let Some(cause) = cause {
        response = warn_needs_build(response, &cause);
    }
    response = crate::diagnosis::warn_ambiguous(response, &candidates);
    if let Some(warning) = axiom_warning {
        response = response.warn(warning);
    }
    if let Some(event) = taint.as_ref().filter(|_| recycled) {
        response = warn_execution_taint(response, event);
    }
    Ok(response)
}

/// Verify explicit, file-wide, or module-wide declaration groups.
///
/// # Errors
///
/// Returns infrastructure failures only. Per-declaration Lean failures are
/// projected into row verdicts.
pub async fn verify_targets(ctx: &ToolContext, req: LeanVerifyRequest) -> Result<Response<LeanVerifyResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let mut expansion = VerifyExpansion::new();

    for (group_index, target_group) in req.targets.iter().enumerate() {
        match target_group {
            LeanVerifyTargetGroup::Explicit { file, declarations } => {
                expansion.push_explicit_group(group_index, file, declarations);
            }
            LeanVerifyTargetGroup::FileAll { file } => {
                let inventory = crate::tools::declaration_inventory::declaration_inventory(
                    ctx,
                    crate::tools::declaration_inventory::DeclarationInventoryRequest {
                        target: crate::tools::declaration_inventory::DeclarationInventoryTarget::File {
                            path: file.clone(),
                        },
                        project: req.project.clone(),
                        limit: Some(MAX_VERIFY_TARGETS),
                    },
                )
                .await?;
                expansion.absorb_inventory_response(&inventory);
                if let Some(result) = inventory.result_ref() {
                    expansion.truncated |= result.truncated;
                    if result.status == "ok" || result.status == NEEDS_BUILD_STATUS {
                        expansion.push_inventory_group(
                            group_index,
                            VerifySource::File(file.clone()),
                            &result.declarations,
                        );
                    } else if let Some(message) = &result.message {
                        expansion
                            .warnings
                            .push(format!("file_all inventory did not produce declarations: {message}"));
                    }
                }
            }
            LeanVerifyTargetGroup::ModuleAll { module } => {
                let inventory = crate::tools::declaration_inventory::declaration_inventory(
                    ctx,
                    crate::tools::declaration_inventory::DeclarationInventoryRequest {
                        target: crate::tools::declaration_inventory::DeclarationInventoryTarget::Module {
                            module: module.clone(),
                        },
                        project: req.project.clone(),
                        limit: Some(MAX_VERIFY_TARGETS),
                    },
                )
                .await?;
                expansion.absorb_inventory_response(&inventory);
                if let Some(result) = inventory.result_ref() {
                    expansion.truncated |= result.truncated;
                    if result.status == "ok" || result.status == NEEDS_BUILD_STATUS {
                        let source = if result.source == "ilean" {
                            VerifySource::ModuleIndex {
                                module: module.clone(),
                                display_file: source_path_for_module(&meta.canonical_root, module),
                            }
                        } else {
                            VerifySource::File(source_path_for_module(&meta.canonical_root, module))
                        };
                        expansion.push_inventory_group(group_index, source, &result.declarations);
                    } else if let Some(message) = &result.message {
                        expansion
                            .warnings
                            .push(format!("module_all inventory did not produce declarations: {message}"));
                    }
                }
            }
            LeanVerifyTargetGroup::Changed {
                base,
                files,
                include_untracked,
            } => {
                let coverage = compute_changed_coverage(
                    ctx,
                    hint.clone(),
                    &meta.canonical_root,
                    ChangedCoverageRequest {
                        base: base.clone(),
                        files: files.clone(),
                        include_untracked: *include_untracked,
                        project: req.project.clone(),
                    },
                )
                .await?;
                expansion.absorb_changed_coverage(&coverage);
                if let Some(result) = coverage.result_ref() {
                    expansion.coverage.extend(result.coverage.clone());
                    expansion.truncated |= result.coverage.truncated;
                    expansion.push_changed_group(group_index, &result.known);
                }
            }
        }
    }

    let requested = expansion.requested;
    if expansion.groups_total_targets() > MAX_VERIFY_TARGETS {
        expansion.truncated = true;
        expansion.truncate(MAX_VERIFY_TARGETS);
    }

    let mut rows = Vec::new();
    let mut last_identity = None;
    let mut build_causes = Vec::new();
    let mut ambiguous = Vec::new();
    let mut axiom_warnings = Vec::new();
    let mut recycled = None;

    let order_by_id = expansion.order_map();
    for group in std::mem::take(&mut expansion.groups) {
        if group.targets.is_empty() {
            continue;
        }
        let prepared = prepare_verify_group(&meta.canonical_root, group)?;
        let source_fact = prepared.source_fact.clone();
        let target_meta = prepared
            .targets
            .iter()
            .map(|target| (target.id.clone(), target.clone()))
            .collect::<HashMap<_, _>>();
        let request = LeanWorkerDeclarationVerificationBatchRequest {
            source: prepared.source,
            targets: prepared
                .targets
                .iter()
                .map(|target| LeanWorkerDeclarationVerificationBatchItem {
                    id: target.id.clone(),
                    target: LeanWorkerDeclarationVerificationTarget::Name {
                        name: target.declaration.clone(),
                    },
                })
                .collect(),
            sorry_policy: if req.allow_sorry {
                LeanWorkerSorryPolicy::Allow
            } else {
                LeanWorkerSorryPolicy::Deny
            },
            report_axioms: req.report_axioms,
            budgets: proof_action_budgets(&ctx.config.output),
        };
        let file_label = prepared.file_label.clone();
        let call = match classify_missing_olean(
            ctx.broker
                .verify_declaration_batch(
                    hint.clone(),
                    session_imports(prepared.imports.clone()),
                    prepared.imports.clone(),
                    request,
                    elab_options(&file_label, ctx.config.output.heartbeat_limit),
                )
                .await,
        )? {
            CallOutcome::Ready(call) => call,
            CallOutcome::NeedsBuild(err) => {
                let base = ctx
                    .broker
                    .project_identity_without_worker(&hint, prepared.imports.clone())?;
                last_identity = Some((base.freshness, base.runtime));
                build_causes.push(IncompleteCause::MissingOlean(err.to_string()));
                rows.extend(prepared.targets.into_iter().map(needs_build_row));
                if let Some(fact) = source_fact {
                    expansion.trust_artifacts.push(fact);
                }
                continue;
            }
        };
        let taint = execution_taint(&call.runtime).cloned();
        last_identity = Some((call.freshness.clone(), call.runtime.clone()));
        if let Some(fact) = source_fact {
            expansion.trust_artifacts.push(fact);
        }
        let projected = project_batch_rows(call.value, &target_meta, req.report_axioms, taint.as_ref());
        if projected.recycled
            && let Some(event) = taint
        {
            recycled = Some(event);
        }
        build_causes.extend(projected.build_causes);
        ambiguous.extend(projected.ambiguous);
        axiom_warnings.extend(projected.axiom_warnings);
        rows.extend(projected.rows);
    }

    rows.sort_by_key(|row| order_by_id.get(&row.id).copied().unwrap_or(usize::MAX));
    let summary = summarize_rows(requested, expansion.truncated, expansion.coverage.unknown.len(), &rows);
    let (freshness, runtime) = match last_identity {
        Some(identity) => identity,
        None => {
            let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
            (base.freshness, base.runtime)
        }
    };
    let mut response = Response::ok(
        LeanVerifyResult {
            summary,
            results: rows,
            coverage: expansion.coverage,
        },
        freshness,
    )
    .with_runtime(runtime)
    .with_trust_artifacts(expansion.trust_artifacts);
    response.warnings.extend(expansion.warnings);
    response.next_actions.extend(expansion.next_actions);
    if req.targets.is_empty() {
        response = response
            .warn("lean_verify received no target groups; no declarations were checked")
            .hint("Provide at least one target group, for example `{\"kind\":\"explicit\",\"file\":\"...\",\"declarations\":[\"...\"]}`.");
    }
    response
        .next_actions
        .push("source files were not modified by verification".to_owned());
    for cause in build_causes {
        response = warn_needs_build(response, &cause);
    }
    response = crate::diagnosis::warn_ambiguous(response, &ambiguous);
    for warning in axiom_warnings {
        if !response.warnings.contains(&warning) {
            response.warnings.push(warning);
        }
    }
    if let Some(event) = recycled.as_ref() {
        response = warn_execution_taint(response, event);
    }
    Ok(response)
}

#[derive(Debug, Clone)]
enum VerifySource {
    File(PathBuf),
    ModuleIndex { module: String, display_file: PathBuf },
}

#[derive(Debug, Clone)]
struct VerifyTarget {
    order: usize,
    id: String,
    file: String,
    declaration: String,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct VerifyGroup {
    source: VerifySource,
    targets: Vec<VerifyTarget>,
}

struct VerifyExpansion {
    groups: Vec<VerifyGroup>,
    group_by_key: HashMap<String, usize>,
    seen_ids: HashSet<String>,
    requested: usize,
    next_order: usize,
    truncated: bool,
    coverage: ChangedCoverageReport,
    trust_artifacts: Vec<ArtifactTrust>,
    warnings: Vec<String>,
    next_actions: Vec<String>,
}

struct VerifyDeclarationItem<'a> {
    name: &'a str,
    reason: Option<String>,
}

impl<'a> From<&'a str> for VerifyDeclarationItem<'a> {
    fn from(name: &'a str) -> Self {
        Self { name, reason: None }
    }
}

impl<'a> From<(&'a str, Option<String>)> for VerifyDeclarationItem<'a> {
    fn from((name, reason): (&'a str, Option<String>)) -> Self {
        Self { name, reason }
    }
}

impl VerifyExpansion {
    fn new() -> Self {
        Self {
            groups: Vec::new(),
            group_by_key: HashMap::new(),
            seen_ids: HashSet::new(),
            requested: 0,
            next_order: 0,
            truncated: false,
            coverage: ChangedCoverageReport::default(),
            trust_artifacts: Vec::new(),
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
    }

    fn push_explicit_group(&mut self, group_index: usize, file: &Path, declarations: &[String]) {
        self.push_declarations(
            group_index,
            VerifySource::File(file.to_path_buf()),
            declarations.iter().map(|name| (name.as_str(), None)),
        );
    }

    fn push_inventory_group(
        &mut self,
        group_index: usize,
        source: VerifySource,
        declarations: &[crate::tools::declaration_inventory::DeclarationInventoryRow],
    ) {
        self.push_declarations(
            group_index,
            source,
            declarations.iter().map(|row| (row.name.as_str(), None)),
        );
    }

    fn push_changed_group(&mut self, group_index: usize, declarations: &[ChangedDeclaration]) {
        let mut by_file = BTreeMap::<String, Vec<(&str, Option<String>)>>::new();
        for declaration in declarations {
            by_file
                .entry(declaration.file.clone())
                .or_default()
                .push((declaration.declaration.as_str(), Some(declaration.reason.clone())));
        }
        for (file, declarations) in by_file {
            self.push_declarations(group_index, VerifySource::File(PathBuf::from(file)), declarations);
        }
    }

    fn push_declarations<'a, I>(&mut self, group_index: usize, source: VerifySource, declarations: I)
    where
        I: IntoIterator,
        I::Item: Into<VerifyDeclarationItem<'a>>,
    {
        let file = file_display(&source);
        let key = source_key(&source);
        let group_idx = match self.group_by_key.get(&key).copied() {
            Some(idx) => idx,
            None => {
                let idx = self.groups.len();
                self.groups.push(VerifyGroup {
                    source,
                    targets: Vec::new(),
                });
                self.group_by_key.insert(key, idx);
                idx
            }
        };
        for declaration in declarations {
            let declaration = declaration.into();
            let name = declaration.name.trim();
            if name.is_empty() {
                self.warnings
                    .push("lean_verify ignored an empty declaration target".to_owned());
                continue;
            }
            self.requested = self.requested.saturating_add(1);
            let mut id = format!("group_{}:{name}", group_index.saturating_add(1));
            if !self.seen_ids.insert(id.clone()) {
                id = format!("{id}#{}", self.next_order.saturating_add(1));
                let _ = self.seen_ids.insert(id.clone());
            }
            if let Some(group) = self.groups.get_mut(group_idx) {
                group.targets.push(VerifyTarget {
                    order: self.next_order,
                    id,
                    file: file.clone(),
                    declaration: name.to_owned(),
                    reason: declaration.reason,
                });
            }
            self.next_order = self.next_order.saturating_add(1);
        }
    }

    fn absorb_inventory_response(
        &mut self,
        response: &Response<crate::tools::declaration_inventory::DeclarationInventoryResult>,
    ) {
        self.trust_artifacts.extend(response.trust_artifacts.clone());
        self.warnings.extend(response.warnings.clone());
        self.next_actions.extend(response.next_actions.clone());
    }

    fn absorb_changed_coverage(&mut self, response: &Response<ChangedCoverageResult>) {
        self.trust_artifacts.extend(response.trust_artifacts.clone());
        self.warnings.extend(response.warnings.clone());
        self.next_actions.extend(response.next_actions.clone());
    }

    fn groups_total_targets(&self) -> usize {
        self.groups.iter().map(|group| group.targets.len()).sum()
    }

    fn truncate(&mut self, limit: usize) {
        let mut remaining = limit;
        for group in &mut self.groups {
            if remaining >= group.targets.len() {
                remaining = remaining.saturating_sub(group.targets.len());
            } else {
                group.targets.truncate(remaining);
                remaining = 0;
            }
        }
    }

    fn order_map(&self) -> HashMap<String, usize> {
        self.groups
            .iter()
            .flat_map(|group| group.targets.iter())
            .map(|target| (target.id.clone(), target.order))
            .collect()
    }
}

struct PreparedVerifyGroup {
    source: String,
    imports: Vec<String>,
    file_label: String,
    source_fact: Option<ArtifactTrust>,
    targets: Vec<VerifyTarget>,
}

fn prepare_verify_group(root: &Path, group: VerifyGroup) -> Result<PreparedVerifyGroup> {
    match group.source {
        VerifySource::File(path) => {
            let input = read_query_file(root, &path)?;
            let file_label = input.resolved.to_string_lossy().into_owned();
            Ok(PreparedVerifyGroup {
                source: input.source,
                imports: input.imports,
                file_label,
                source_fact: Some(ArtifactTrust::source_file_edit_fresh(root, &input.resolved)),
                targets: group.targets,
            })
        }
        VerifySource::ModuleIndex { module, display_file } => Ok(PreparedVerifyGroup {
            source: String::new(),
            imports: vec![module],
            file_label: display_file.to_string_lossy().into_owned(),
            source_fact: None,
            targets: group.targets,
        }),
    }
}

fn source_key(source: &VerifySource) -> String {
    match source {
        VerifySource::File(path) => format!("file:{}", path.to_string_lossy()),
        VerifySource::ModuleIndex { module, .. } => format!("module_index:{module}"),
    }
}

fn file_display(source: &VerifySource) -> String {
    match source {
        VerifySource::File(path) => path.to_string_lossy().into_owned(),
        VerifySource::ModuleIndex { display_file, .. } => display_file.to_string_lossy().into_owned(),
    }
}

struct ProjectedBatchRows {
    rows: Vec<LeanVerifyRow>,
    build_causes: Vec<IncompleteCause>,
    ambiguous: Vec<crate::diagnosis::CompetingDecl>,
    axiom_warnings: Vec<String>,
    recycled: bool,
}

fn project_batch_rows(
    result: LeanWorkerDeclarationVerificationBatchResult,
    target_meta: &HashMap<String, VerifyTarget>,
    report_axioms: bool,
    taint: Option<&crate::envelope::RuntimeRestartEvent>,
) -> ProjectedBatchRows {
    match result {
        LeanWorkerDeclarationVerificationBatchResult::Ok { results, imports } => {
            project_batch_verdict_rows(results, imports, None, target_meta, report_axioms, taint)
        }
        LeanWorkerDeclarationVerificationBatchResult::MissingImports {
            results,
            imports,
            missing,
        } => project_batch_verdict_rows(results, imports, Some(missing), target_meta, report_axioms, taint),
        LeanWorkerDeclarationVerificationBatchResult::HeaderParseFailed { diagnostics } => {
            let diagnostics = crate::projections::project_failure(&diagnostics);
            ProjectedBatchRows {
                rows: target_meta
                    .values()
                    .map(|target| LeanVerifyRow {
                        id: target.id.clone(),
                        file: target.file.clone(),
                        declaration: target.declaration.clone(),
                        reason: target.reason.clone(),
                        verification_status: "header_parse_failed".to_owned(),
                        facts: None,
                        missing_imports: Vec::new(),
                        diagnostics: Some(diagnostics.clone()),
                    })
                    .collect(),
                build_causes: Vec::new(),
                ambiguous: Vec::new(),
                axiom_warnings: Vec::new(),
                recycled: false,
            }
        }
        LeanWorkerDeclarationVerificationBatchResult::Unsupported => ProjectedBatchRows {
            rows: target_meta
                .values()
                .map(|target| LeanVerifyRow {
                    id: target.id.clone(),
                    file: target.file.clone(),
                    declaration: target.declaration.clone(),
                    reason: target.reason.clone(),
                    verification_status: "unsupported".to_owned(),
                    facts: None,
                    missing_imports: Vec::new(),
                    diagnostics: None,
                })
                .collect(),
            build_causes: Vec::new(),
            ambiguous: Vec::new(),
            axiom_warnings: Vec::new(),
            recycled: false,
        },
        _ => ProjectedBatchRows {
            rows: target_meta
                .values()
                .map(|target| LeanVerifyRow {
                    id: target.id.clone(),
                    file: target.file.clone(),
                    declaration: target.declaration.clone(),
                    reason: target.reason.clone(),
                    verification_status: "unsupported".to_owned(),
                    facts: None,
                    missing_imports: Vec::new(),
                    diagnostics: None,
                })
                .collect(),
            build_causes: Vec::new(),
            ambiguous: Vec::new(),
            axiom_warnings: Vec::new(),
            recycled: false,
        },
    }
}

fn project_batch_verdict_rows(
    rows: Vec<LeanWorkerDeclarationVerificationBatchRow>,
    imports: Vec<String>,
    missing: Option<Vec<String>>,
    target_meta: &HashMap<String, VerifyTarget>,
    report_axioms: bool,
    taint: Option<&crate::envelope::RuntimeRestartEvent>,
) -> ProjectedBatchRows {
    let mut out = ProjectedBatchRows {
        rows: Vec::with_capacity(rows.len()),
        build_causes: Vec::new(),
        ambiguous: Vec::new(),
        axiom_warnings: Vec::new(),
        recycled: false,
    };
    if let Some(missing) = missing.as_ref() {
        out.build_causes.push(IncompleteCause::MissingImports(missing.clone()));
    }
    for row in rows {
        let Some(target) = target_meta.get(&row.id) else {
            continue;
        };
        let mut projected = match missing.clone() {
            Some(missing) => project_declaration_verification(WorkerDeclarationVerificationResult::MissingImports {
                verification_status: row.verification_status,
                facts: row.facts,
                imports: imports.clone(),
                missing,
            }),
            None => project_declaration_verification(WorkerDeclarationVerificationResult::Ok {
                verification_status: row.verification_status,
                facts: row.facts,
                imports: imports.clone(),
            }),
        };
        if taint.is_some() && relabel_recycled_verdict(&mut projected) {
            out.recycled = true;
        }
        if let Some(cause) = verification_incomplete_cause(&projected) {
            out.build_causes.push(cause);
        }
        out.ambiguous.extend(verification_ambiguous_candidates(&projected));
        if let Some(warning) = axiom_unavailable_warning(&projected, report_axioms) {
            out.axiom_warnings.push(warning);
        }
        out.rows.push(row_from_projected(target, projected));
    }
    out
}

fn row_from_projected(target: &VerifyTarget, result: DeclarationVerificationResult) -> LeanVerifyRow {
    match result {
        DeclarationVerificationResult::Ok {
            verification_status,
            facts,
            ..
        } => LeanVerifyRow {
            id: target.id.clone(),
            file: target.file.clone(),
            declaration: target.declaration.clone(),
            reason: target.reason.clone(),
            verification_status,
            facts: Some(facts),
            missing_imports: Vec::new(),
            diagnostics: None,
        },
        DeclarationVerificationResult::MissingImports {
            verification_status,
            facts,
            missing,
            ..
        } => LeanVerifyRow {
            id: target.id.clone(),
            file: target.file.clone(),
            declaration: target.declaration.clone(),
            reason: target.reason.clone(),
            verification_status,
            facts: Some(facts),
            missing_imports: missing,
            diagnostics: None,
        },
        DeclarationVerificationResult::HeaderParseFailed { diagnostics } => LeanVerifyRow {
            id: target.id.clone(),
            file: target.file.clone(),
            declaration: target.declaration.clone(),
            reason: target.reason.clone(),
            verification_status: "header_parse_failed".to_owned(),
            facts: None,
            missing_imports: Vec::new(),
            diagnostics: Some(diagnostics),
        },
        DeclarationVerificationResult::Unsupported => LeanVerifyRow {
            id: target.id.clone(),
            file: target.file.clone(),
            declaration: target.declaration.clone(),
            reason: target.reason.clone(),
            verification_status: "unsupported".to_owned(),
            facts: None,
            missing_imports: Vec::new(),
            diagnostics: None,
        },
    }
}

fn needs_build_row(target: VerifyTarget) -> LeanVerifyRow {
    LeanVerifyRow {
        id: target.id,
        file: target.file,
        declaration: target.declaration,
        reason: target.reason,
        verification_status: NEEDS_BUILD_STATUS.to_owned(),
        facts: Some(Box::new(needs_build_facts())),
        missing_imports: Vec::new(),
        diagnostics: None,
    }
}

fn summarize_rows(
    requested: usize,
    truncated: bool,
    unknown_coverage: usize,
    rows: &[LeanVerifyRow],
) -> LeanVerifySummary {
    let verified = rows.iter().filter(|row| row.verification_status == "verified").count();
    let needs_build = rows
        .iter()
        .filter(|row| row.verification_status == NEEDS_BUILD_STATUS)
        .count();
    LeanVerifySummary {
        requested,
        verified,
        failed: rows.len().saturating_sub(verified).saturating_sub(needs_build),
        needs_build,
        unknown_coverage,
        truncated,
    }
}

/// When the worker was recycled mid-call, a non-positive verification verdict is
/// suspect: relabel it to `worker_recycled` with untrustworthy facts. Returns
/// `true` if it relabeled. Leaves `verified` (still trustworthy — verification
/// is monotone) and the already-honest `needs_build` / `ambiguous` verdicts
/// unchanged, and only touches the `Ok` variant — a `MissingImports` verdict's
/// honest action is `lake build`, not a recycle notice. Pure, for unit testing.
fn relabel_recycled_verdict(result: &mut DeclarationVerificationResult) -> bool {
    let DeclarationVerificationResult::Ok {
        verification_status,
        facts,
        ..
    } = result
    else {
        return false;
    };
    let status = verification_status.as_str();
    // `verified` is monotone-trustworthy; `needs_build` / `ambiguous` carry their
    // own honest verdict; and the relabel is idempotent (already `worker_recycled`).
    if status == "verified" || status == NEEDS_BUILD_STATUS || status == "ambiguous" || status == WORKER_RECYCLED_STATUS
    {
        return false;
    }
    WORKER_RECYCLED_STATUS.clone_into(verification_status);
    facts.facts_trustworthy = false;
    true
}

/// Build the degraded verdict + envelope when `verify_declaration`'s target
/// import closure hit an unbuilt `.olean`. Freshness/runtime come from the
/// non-spawning broker identity path, so only this rare arm pays for it.
fn verification_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    source_fact: ArtifactTrust,
    err: ServerError,
) -> Result<Response<DeclarationVerificationResult>> {
    let base = ctx.broker.project_identity_without_worker(&hint, imports.clone())?;
    let mut response = Response::ok(needs_build_verification_result(imports), base.freshness)
        .with_runtime(base.runtime)
        .with_trust_artifact(source_fact);
    response
        .next_actions
        .push("source file was not modified by verification".to_owned());
    Ok(warn_needs_build(
        response,
        &IncompleteCause::MissingOlean(err.to_string()),
    ))
}

/// The verification verdict for an unbuilt-dependency degrade. Same wire shape
/// as the worker-typed `needs_build` (status `missing_imports`,
/// `verification_status:"needs_build"`, `facts_trustworthy:false`) so the two
/// degrade paths are indistinguishable to a client. Pure, for unit testing.
fn needs_build_verification_result(imports: Vec<String>) -> DeclarationVerificationResult {
    DeclarationVerificationResult::MissingImports {
        verification_status: NEEDS_BUILD_STATUS.to_owned(),
        facts: Box::new(needs_build_facts()),
        imports,
        missing: Vec::new(),
    }
}

/// Untrustworthy, empty facts for a degraded verdict: nothing was checked
/// because the environment could not be assembled. `axioms_available:false`
/// reads the empty `axioms` as "not computed", not "no axioms".
fn needs_build_facts() -> DeclarationVerificationFacts {
    DeclarationVerificationFacts {
        target: None,
        diagnostics: ElabFailure {
            diagnostics: Vec::new(),
            truncated: false,
        },
        unresolved_goals: Vec::new(),
        contains_sorry: false,
        contains_admit: false,
        contains_sorry_ax: false,
        axioms: Vec::new(),
        axioms_truncated: false,
        axioms_available: false,
        output_truncated: false,
        candidates: Vec::new(),
        facts_trustworthy: false,
    }
}

/// Incomplete-build cause for a verification result, if the verdict was
/// computed against an environment that was not fully assembled.
fn verification_incomplete_cause(result: &DeclarationVerificationResult) -> Option<IncompleteCause> {
    match result {
        // The worker reports needs_build through the MissingImports outcome,
        // which names the unbuilt modules.
        DeclarationVerificationResult::MissingImports { missing, .. } => {
            Some(IncompleteCause::MissingImports(missing.clone()))
        }
        DeclarationVerificationResult::Ok {
            verification_status, ..
        } if verification_status == NEEDS_BUILD_STATUS => Some(IncompleteCause::MissingImports(Vec::new())),
        DeclarationVerificationResult::Ok { .. }
        | DeclarationVerificationResult::HeaderParseFailed { .. }
        | DeclarationVerificationResult::Unsupported => None,
    }
}

/// Competing declarations when the verdict is genuinely ambiguous, ready for
/// the shared ambiguity renderer. Empty otherwise.
fn verification_ambiguous_candidates(result: &DeclarationVerificationResult) -> Vec<crate::diagnosis::CompetingDecl> {
    let DeclarationVerificationResult::Ok {
        verification_status,
        facts,
        ..
    } = result
    else {
        return Vec::new();
    };
    if verification_status != "ambiguous" {
        return Vec::new();
    }
    facts
        .candidates
        .iter()
        .map(|c| crate::diagnosis::CompetingDecl {
            name: c.declaration_name.clone(),
            namespace: (!c.namespace_name.is_empty()).then(|| c.namespace_name.clone()),
        })
        .collect()
}

/// When `report_axioms` was requested but the worker could not compute the
/// axiom set (`axioms_available == false`), the empty `axioms` list means "not
/// computed", not "no axioms". Say so. A genuine empty set
/// (`axioms_available == true`) needs no caveat — the false-positive is defined
/// out of existence.
fn axiom_unavailable_warning(result: &DeclarationVerificationResult, report_axioms: bool) -> Option<String> {
    if !report_axioms {
        return None;
    }
    let DeclarationVerificationResult::Ok { facts, .. } = result else {
        return None;
    };
    (!facts.axioms_available).then(|| {
        "report_axioms: the axiom dependency set could not be computed (target unresolved or budget exhausted); \
         the empty `axioms` list means \"not computed\", not \"no axioms\""
            .to_owned()
    })
}

fn proof_action_budgets(output: &OutputBudgetOverrides) -> LeanWorkerOutputBudgets {
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

fn elab_options(file_label: &str, heartbeat_limit: Option<u64>) -> LeanWorkerElabOptions {
    let options = LeanWorkerElabOptions::new().file_label(file_label);
    match heartbeat_limit {
        Some(limit) => options.heartbeat_limit(limit),
        None => options,
    }
}

fn proof_candidates(req: &TryProofStepRequest) -> Vec<LeanWorkerProofCandidate> {
    requested_snippets(req)
        .into_iter()
        .enumerate()
        .map(|(idx, text)| LeanWorkerProofCandidate {
            id: format!("candidate_{}", idx.saturating_add(1)),
            text,
        })
        .collect()
}

fn requested_snippets(req: &TryProofStepRequest) -> Vec<String> {
    let mut snippets = Vec::new();
    if let Some(snippet) = req.snippet.as_ref().filter(|text| !text.trim().is_empty()) {
        snippets.push(snippet.clone());
    }
    snippets.extend(req.snippets.iter().filter(|text| !text.trim().is_empty()).cloned());
    snippets
}

fn empty_proof_attempt_envelope() -> ProofAttemptEnvelope {
    let mut envelope = ProofAttemptEnvelope {
        candidates: Vec::new(),
        candidate_limit: MAX_CANDIDATES as u32,
        candidates_truncated: false,
        summary: crate::projections::ProofAttemptSummary {
            requested_candidates: 0,
            returned_candidates: 0,
            candidate_limit: MAX_CANDIDATES as u32,
            candidates_truncated: false,
            partial: false,
            closed: 0,
            progressed: 0,
            failed: 0,
            timeout: 0,
            budget_exceeded: 0,
            not_attempted: 0,
            unsupported: 0,
            output_truncated: 0,
        },
    };
    envelope.refresh_summary(0);
    envelope
}

fn refresh_proof_attempt_summary(result: ProofAttemptResult, requested_count: usize) -> ProofAttemptResult {
    match result {
        ProofAttemptResult::Ok { result, imports } => ProofAttemptResult::Ok {
            result: refresh_envelope_summary(result, requested_count),
            imports,
        },
        ProofAttemptResult::MissingImports {
            result,
            imports,
            missing,
        } => ProofAttemptResult::MissingImports {
            result: refresh_envelope_summary(result, requested_count),
            imports,
            missing,
        },
        ProofAttemptResult::HeaderParseFailed { diagnostics } => ProofAttemptResult::HeaderParseFailed { diagnostics },
        ProofAttemptResult::Unsupported => ProofAttemptResult::Unsupported,
    }
}

fn refresh_envelope_summary(mut envelope: ProofAttemptEnvelope, requested_count: usize) -> ProofAttemptEnvelope {
    envelope.refresh_summary(requested_count);
    envelope
}

fn warn_partial_proof_attempt(response: &mut Response<ProofAttemptResult>) {
    let Some(summary) = response.result_ref().and_then(proof_attempt_summary).cloned() else {
        return;
    };
    if summary.candidates_truncated {
        response.warnings.push(format!(
            "proof candidate batch was truncated at {} of {} requested snippets",
            summary.returned_candidates, summary.requested_candidates
        ));
        response
            .next_actions
            .push("submit fewer proof snippets if you need a verdict for every candidate".to_owned());
    }
    if summary.budget_exceeded > 0 || summary.not_attempted > 0 || summary.output_truncated > 0 {
        response.warnings.push(format!(
            "proof candidate batch returned partial output: budget_exceeded={}, not_attempted={}, output_truncated={}",
            summary.budget_exceeded, summary.not_attempted, summary.output_truncated
        ));
        response.next_actions.push(
            "retry promising snippets individually or raise output.max_total_bytes for a larger batch response"
                .to_owned(),
        );
    }
}

fn proof_attempt_summary(result: &ProofAttemptResult) -> Option<&crate::projections::ProofAttemptSummary> {
    match result {
        ProofAttemptResult::Ok { result, .. } | ProofAttemptResult::MissingImports { result, .. } => {
            Some(&result.summary)
        }
        ProofAttemptResult::HeaderParseFailed { .. } | ProofAttemptResult::Unsupported => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::panic,
        reason = "unit tests should fail directly on malformed fixtures"
    )]

    use serde_json::json;

    use super::*;
    use crate::broker::{BrokerConfig, ProjectBroker};
    use crate::tools::{ToolConfig, ToolContext};
    use crate::trust::{ArtifactKind, TrustScope, TrustStatus};

    fn make_lake_dir(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("proof_action");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lakefile.lean"), "package proof_action\nlean_lib Demo\n").unwrap();
        std::fs::write(dir.join("lean-toolchain"), "leanprover/lean4:v4.31.0-rc2\n").unwrap();
        std::fs::write(dir.join("lake-manifest.json"), "{}\n").unwrap();
        std::fs::write(dir.join("Demo.lean"), "import Init\nexample : True := by trivial\n").unwrap();
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

    fn assert_source_edit_fresh(response: &Response<impl serde::Serialize + JsonSchema>) {
        assert!(response.trust_artifacts.iter().any(|artifact| {
            artifact.artifact == ArtifactKind::Source
                && artifact.scope == TrustScope::File
                && artifact.status == TrustStatus::EditFresh
                && artifact.path.as_deref() == Some("Demo.lean")
        }));
    }

    #[test]
    fn try_proof_step_request_accepts_single_snippet() {
        let req: TryProofStepRequest = serde_json::from_value(json!({
            "file": "Demo.lean",
            "declaration": "Demo.closed",
            "snippet": "rfl"
        }))
        .unwrap();
        assert_eq!(requested_snippets(&req), vec!["rfl"]);
    }

    #[test]
    fn try_proof_step_request_accepts_snippet_list_for_upstream_batching() {
        let snippets = (0..20).map(|idx| format!("exact h{idx}")).collect::<Vec<_>>();
        let req = TryProofStepRequest {
            file: PathBuf::from("Demo.lean"),
            declaration: "Demo.closed".to_owned(),
            proof_position: ProofPositionSelector::Default,
            project: None,
            snippet: None,
            snippets,
        };
        let candidates = proof_candidates(&req);
        assert_eq!(candidates.len(), 20);
        assert_eq!(candidates[MAX_CANDIDATES].id, "candidate_17");
    }

    #[test]
    fn proof_step_partial_summary_counts_upstream_not_attempted_rows() {
        use crate::projections::{ProofAttemptCandidate, RenderedText};
        let candidate = |id: &str, status: &str, output_truncated: bool| ProofAttemptCandidate {
            id: id.to_owned(),
            status: status.to_owned(),
            snippet: RenderedText {
                value: "trivial".to_owned(),
                truncated: false,
            },
            diagnostics: ElabFailure {
                diagnostics: Vec::new(),
                truncated: false,
            },
            downstream_diagnostics: ElabFailure {
                diagnostics: Vec::new(),
                truncated: false,
            },
            goals: Vec::new(),
            declaration: None,
            proof_position: None,
            output_truncated,
        };
        let result = refresh_proof_attempt_summary(
            ProofAttemptResult::Ok {
                result: ProofAttemptEnvelope {
                    candidates: vec![
                        candidate("candidate_1", "closed", false),
                        candidate("candidate_2", "budget_exceeded", true),
                        candidate("candidate_3", "not_attempted", false),
                    ],
                    candidate_limit: MAX_CANDIDATES as u32,
                    candidates_truncated: false,
                    summary: empty_proof_attempt_envelope().summary,
                },
                imports: Vec::new(),
            },
            3,
        );
        let Some(summary) = proof_attempt_summary(&result) else {
            panic!("proof attempt should have a summary");
        };
        assert_eq!(summary.requested_candidates, 3);
        assert_eq!(summary.returned_candidates, 3);
        assert_eq!(summary.closed, 1);
        assert_eq!(summary.budget_exceeded, 1);
        assert_eq!(summary.not_attempted, 1);
        assert_eq!(summary.output_truncated, 1);
        assert!(summary.partial);
    }

    #[tokio::test]
    async fn try_proof_step_marks_read_source_snapshot_edit_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        let (ctx, broker) = test_context(root);
        let response = try_proof_step(
            &ctx,
            TryProofStepRequest {
                file: PathBuf::from("Demo.lean"),
                declaration: "Demo.example".to_owned(),
                proof_position: ProofPositionSelector::Default,
                project: None,
                snippet: None,
                snippets: Vec::new(),
            },
        )
        .await
        .unwrap();

        assert_source_edit_fresh(&response);
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn binder_reintroduction_diagnostics_drive_the_cue() {
        use crate::projections::{CoordinateSpace, Diagnostic, Severity};
        let with = |message: &str| ElabFailure {
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                message: message.to_owned(),
                coordinate_space: CoordinateSpace::Unknown,
                position: None,
                original_range: None,
                synthetic_range: None,
                file: None,
                coordinate_note: None,
            }],
            truncated: false,
        };
        // The exact message Lean 4.31 emits for this fixture, plus an older phrasing.
        assert!(diagnostics_reintroduce_binders(&with(
            "Tactic `introN` failed: There are no additional binders or `let` bindings in the goal to introduce"
        )));
        assert!(diagnostics_reintroduce_binders(&with(
            "tactic 'introN' failed, insufficient number of binders"
        )));
        assert!(!diagnostics_reintroduce_binders(&with("unknown identifier 'foo'")));
    }

    #[test]
    fn verify_declaration_request_accepts_declaration_mode() {
        let req: VerifyDeclarationRequest = serde_json::from_value(json!({
            "file": "Demo.lean",
            "declaration": "Demo.closed",
            "report_axioms": true
        }))
        .unwrap();
        assert_eq!(req.declaration, "Demo.closed");
        assert!(req.report_axioms);
    }

    #[test]
    fn lean_verify_request_accepts_target_groups() {
        let req: LeanVerifyRequest = serde_json::from_value(json!({
            "targets": [
                {
                    "kind": "explicit",
                    "file": "Demo.lean",
                    "declarations": ["Demo.closed", "Demo.other"]
                },
                { "kind": "file_all", "file": "Other.lean" },
                { "kind": "module_all", "module": "Demo.Other" }
            ],
            "allow_sorry": true,
            "report_axioms": true
        }))
        .unwrap();
        assert_eq!(req.targets.len(), 3);
        assert!(req.allow_sorry);
        assert!(req.report_axioms);
    }

    #[test]
    fn lean_verify_summary_separates_needs_build_from_failures() {
        let rows = vec![
            LeanVerifyRow {
                id: "group_1:Demo.ok".to_owned(),
                file: "Demo.lean".to_owned(),
                declaration: "Demo.ok".to_owned(),
                reason: None,
                verification_status: "verified".to_owned(),
                facts: None,
                missing_imports: Vec::new(),
                diagnostics: None,
            },
            LeanVerifyRow {
                id: "group_1:Demo.sorry".to_owned(),
                file: "Demo.lean".to_owned(),
                declaration: "Demo.sorry".to_owned(),
                reason: None,
                verification_status: "has_sorry".to_owned(),
                facts: None,
                missing_imports: Vec::new(),
                diagnostics: None,
            },
            LeanVerifyRow {
                id: "group_2:Demo.unbuilt".to_owned(),
                file: "Demo.lean".to_owned(),
                declaration: "Demo.unbuilt".to_owned(),
                reason: None,
                verification_status: NEEDS_BUILD_STATUS.to_owned(),
                facts: None,
                missing_imports: Vec::new(),
                diagnostics: None,
            },
        ];

        let summary = summarize_rows(4, true, 2, &rows);
        assert_eq!(summary.requested, 4);
        assert_eq!(summary.verified, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.needs_build, 1);
        assert_eq!(summary.unknown_coverage, 2);
        assert!(summary.truncated);
    }

    #[test]
    fn lean_verify_expansion_coalesces_targets_by_source() {
        let mut expansion = VerifyExpansion::new();
        expansion.push_explicit_group(
            0,
            std::path::Path::new("Demo.lean"),
            &["Demo.a".to_owned(), "Demo.b".to_owned()],
        );
        expansion.push_explicit_group(1, std::path::Path::new("Demo.lean"), &["Demo.c".to_owned()]);

        assert_eq!(expansion.groups.len(), 1);
        assert_eq!(expansion.groups[0].targets.len(), 3);
        assert_eq!(expansion.requested, 3);
        assert_eq!(
            expansion.groups[0]
                .targets
                .iter()
                .map(|target| target.declaration.as_str())
                .collect::<Vec<_>>(),
            ["Demo.a", "Demo.b", "Demo.c"]
        );
    }

    #[tokio::test]
    async fn verify_declaration_marks_read_source_snapshot_edit_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        let (ctx, broker) = test_context(root);
        let response = verify_declaration(
            &ctx,
            VerifyDeclarationRequest {
                file: PathBuf::from("Demo.lean"),
                declaration: String::new(),
                project: None,
                allow_sorry: false,
                report_axioms: false,
            },
        )
        .await
        .unwrap();

        assert_source_edit_fresh(&response);
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[tokio::test]
    async fn lean_verify_empty_targets_warns_that_nothing_was_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        let (ctx, broker) = test_context(root);

        let response = verify_targets(
            &ctx,
            LeanVerifyRequest {
                targets: Vec::new(),
                project: None,
                allow_sorry: false,
                report_axioms: false,
            },
        )
        .await
        .unwrap();

        let result = response.result_ref().unwrap();
        assert_eq!(result.summary.requested, 0);
        assert!(result.results.is_empty());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("no target groups"))
        );
        assert!(
            response
                .next_actions
                .iter()
                .any(|action| action.contains("\"kind\":\"explicit\""))
        );
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn proof_action_budget_clamps() {
        let low = proof_action_budgets(&OutputBudgetOverrides {
            max_field_bytes: Some(1),
            max_total_bytes: Some(1),
            heartbeat_limit: None,
        });
        assert_eq!(low.per_field_bytes, MIN_FIELD_BYTES);
        assert_eq!(low.total_bytes, MIN_TOTAL_BYTES);

        let high = proof_action_budgets(&OutputBudgetOverrides {
            max_field_bytes: Some(u32::MAX),
            max_total_bytes: Some(u32::MAX),
            heartbeat_limit: None,
        });
        assert_eq!(high.per_field_bytes, MAX_FIELD_BYTES);
        assert_eq!(high.total_bytes, MAX_TOTAL_BYTES);

        let default = proof_action_budgets(&OutputBudgetOverrides::default());
        assert_eq!(default.per_field_bytes, DEFAULT_FIELD_BYTES);
        assert_eq!(default.total_bytes, DEFAULT_TOTAL_BYTES);
    }

    #[test]
    fn unbuilt_dependency_verification_is_needs_build_with_untrustworthy_facts() {
        // The env-based degrade must match the worker-typed needs_build shape:
        // verification_status "needs_build" + facts_trustworthy false.
        let DeclarationVerificationResult::MissingImports {
            verification_status,
            facts,
            ..
        } = needs_build_verification_result(vec!["Foo.Bar".to_owned()])
        else {
            panic!("expected a missing_imports verdict");
        };
        assert_eq!(verification_status, NEEDS_BUILD_STATUS);
        assert!(!facts.facts_trustworthy);
        assert!(!facts.axioms_available);
        assert!(!facts.contains_sorry);
    }

    fn ok_verdict(status: &str, trustworthy: bool) -> DeclarationVerificationResult {
        let mut facts = needs_build_facts();
        facts.facts_trustworthy = trustworthy;
        DeclarationVerificationResult::Ok {
            verification_status: status.to_owned(),
            facts: Box::new(facts),
            imports: Vec::new(),
        }
    }

    #[test]
    fn recycled_relabels_nonpositive_ok_verdict_to_worker_recycled() {
        // A `not_found` produced while the worker was recycled is a likely
        // casualty of the recycle, not a real "name absent".
        let mut verdict = ok_verdict("not_found", true);
        assert!(relabel_recycled_verdict(&mut verdict));
        let DeclarationVerificationResult::Ok {
            verification_status,
            facts,
            ..
        } = verdict
        else {
            panic!("expected an Ok verdict");
        };
        assert_eq!(verification_status, WORKER_RECYCLED_STATUS);
        assert!(!facts.facts_trustworthy);
    }

    #[test]
    fn recycled_leaves_verified_and_already_honest_verdicts_unchanged() {
        // `verified` is monotone-trustworthy even under duress; needs_build and
        // ambiguous already carry their own honest, actionable verdict.
        for status in ["verified", NEEDS_BUILD_STATUS, "ambiguous"] {
            let mut verdict = ok_verdict(status, true);
            assert!(
                !relabel_recycled_verdict(&mut verdict),
                "{status} must not be relabeled"
            );
            let DeclarationVerificationResult::Ok {
                verification_status,
                facts,
                ..
            } = verdict
            else {
                panic!("expected an Ok verdict");
            };
            assert_eq!(verification_status, status);
            assert!(facts.facts_trustworthy);
        }
    }

    #[test]
    fn recycled_does_not_touch_missing_imports_verdict() {
        // A MissingImports verdict's honest action is `lake build`, owned by the
        // needs_build path — not a recycle notice.
        let mut verdict = needs_build_verification_result(vec!["Foo.Bar".to_owned()]);
        assert!(!relabel_recycled_verdict(&mut verdict));
    }

    #[test]
    fn unbuilt_dependency_proof_attempt_is_empty_missing_imports() {
        let ProofAttemptResult::MissingImports { result, missing, .. } =
            needs_build_attempt_result(vec!["Foo.Bar".to_owned()])
        else {
            panic!("expected a missing_imports envelope");
        };
        assert!(result.candidates.is_empty());
        assert!(missing.is_empty());
    }
}
