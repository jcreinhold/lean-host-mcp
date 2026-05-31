//! Non-mutating proof action tools.
//!
//! `try_proof_step` and `verify_declaration` read a Lean file, send its
//! contents to the worker as an in-memory overlay, and return structured
//! proof/verification outcomes. They never write source files.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::PathBuf;

use lean_rs_worker_parent::{
    LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationTarget, LeanWorkerElabOptions,
    LeanWorkerOutputBudgets, LeanWorkerProofAttemptRequest, LeanWorkerProofCandidate, LeanWorkerProofEditTarget,
    LeanWorkerSorryPolicy,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::broker::ProjectHint;
use crate::diagnosis::{
    CallOutcome, IncompleteCause, NEEDS_BUILD_STATUS, WORKER_RECYCLED_STATUS, classify_missing_olean, execution_taint,
    warn_execution_taint, warn_needs_build,
};
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::projections::{
    DeclarationVerificationFacts, DeclarationVerificationResult, ElabFailure, ProofAttemptCandidate,
    ProofAttemptEnvelope, ProofAttemptResult, project_declaration_verification, project_proof_attempt,
};
use crate::tools::position::{ProofPositionSelector, worker_proof_position};
use crate::tools::source_input::read_query_file;
use crate::tools::{ToolContext, session_imports};

const MAX_CANDIDATES: usize = 8;
const DEFAULT_FIELD_BYTES: u32 = 4 * 1024;
const MIN_FIELD_BYTES: u32 = 256;
const MAX_FIELD_BYTES: u32 = 64 * 1024;
const DEFAULT_TOTAL_BYTES: u32 = 64 * 1024;
const MIN_TOTAL_BYTES: u32 = 1024;
const MAX_TOTAL_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TryProofStepRequest {
    pub file: PathBuf,
    pub declaration: String,
    #[serde(default)]
    pub proof_position: ProofPositionSelector,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub snippets: Vec<String>,
    #[serde(default)]
    pub max_field_bytes: Option<u32>,
    #[serde(default)]
    pub max_total_bytes: Option<u32>,
    #[serde(default)]
    pub heartbeat_limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct VerifyDeclarationRequest {
    pub file: PathBuf,
    pub declaration: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub allow_sorry: bool,
    #[serde(default)]
    pub report_axioms: bool,
    #[serde(default)]
    pub max_field_bytes: Option<u32>,
    #[serde(default)]
    pub max_total_bytes: Option<u32>,
    #[serde(default)]
    pub heartbeat_limit: Option<u64>,
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
    let file_label = input.resolved.to_string_lossy().into_owned();
    let budgets = proof_action_budgets(req.max_field_bytes, req.max_total_bytes);
    let candidates = proof_candidates(&req);
    let extra_rows = capped_candidate_rows(&req);

    if candidates.is_empty() {
        let runtime = ctx.broker.project_runtime(hint, input.imports.clone()).await?;
        return Ok(Response::ok(
            ProofAttemptResult::Ok {
                result: ProofAttemptEnvelope {
                    candidates: extra_rows,
                    candidate_limit: MAX_CANDIDATES as u32,
                    candidates_truncated: false,
                },
                imports: input.imports,
            },
            runtime.freshness,
        )
        .with_runtime(runtime.runtime)
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
                elab_options(&file_label, req.heartbeat_limit),
            )
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return proof_step_needs_build_response(ctx, hint, imports, err).await,
    };
    let taint = execution_taint(&call.runtime).cloned();
    let mut response = Response::ok(
        append_capped_rows(project_proof_attempt(call.value), extra_rows),
        call.freshness,
    )
    .with_runtime(call.runtime);
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
    Ok(response)
}

/// Build the degraded envelope when `try_proof_step`'s target import closure
/// hit an unbuilt `.olean`: no candidate could run against an incomplete
/// environment. Mirrors the verify degrade — a `missing_imports` result plus
/// the canonical `needs_build` warning naming the blocking olean.
async fn proof_step_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    err: ServerError,
) -> Result<Response<ProofAttemptResult>> {
    let base = ctx.broker.project_runtime(hint, imports.clone()).await?;
    let mut response = Response::ok(needs_build_attempt_result(imports), base.freshness).with_runtime(base.runtime);
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
        result: ProofAttemptEnvelope {
            candidates: Vec::new(),
            candidate_limit: MAX_CANDIDATES as u32,
            candidates_truncated: false,
        },
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
    let file_label = input.resolved.to_string_lossy().into_owned();
    let budgets = proof_action_budgets(req.max_field_bytes, req.max_total_bytes);
    if req.declaration.trim().is_empty() {
        let runtime = ctx.broker.project_runtime(hint, input.imports.clone()).await?;
        return Ok(
            Response::ok(DeclarationVerificationResult::Unsupported, runtime.freshness)
                .with_runtime(runtime.runtime)
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
                elab_options(&file_label, req.heartbeat_limit),
            )
            .await,
    )? {
        CallOutcome::Ready(call) => call,
        CallOutcome::NeedsBuild(err) => return verification_needs_build_response(ctx, hint, imports, err).await,
    };
    // If the worker was recycled/crashed mid-call, a non-positive verdict is a
    // likely casualty of the recycle, not a real result; relabel it honestly
    // before it reaches the agent (verification is monotone, so a `verified`
    // verdict is left trustworthy even under duress).
    let taint = execution_taint(&call.runtime).cloned();
    let mut result = project_declaration_verification(call.value);
    let recycled = taint.is_some() && relabel_recycled_verdict(&mut result);
    let mut response = Response::ok(result, call.freshness).with_runtime(call.runtime);
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
/// import closure hit an unbuilt `.olean`. Freshness/runtime come from
/// [`crate::broker::ProjectBroker::project_runtime`], a registry hit with no
/// worker round-trip, so only this rare arm pays for it.
async fn verification_needs_build_response(
    ctx: &ToolContext,
    hint: ProjectHint,
    imports: Vec<String>,
    err: ServerError,
) -> Result<Response<DeclarationVerificationResult>> {
    let base = ctx.broker.project_runtime(hint, imports.clone()).await?;
    let mut response =
        Response::ok(needs_build_verification_result(imports), base.freshness).with_runtime(base.runtime);
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

fn proof_action_budgets(max_field_bytes: Option<u32>, max_total_bytes: Option<u32>) -> LeanWorkerOutputBudgets {
    LeanWorkerOutputBudgets {
        per_field_bytes: max_field_bytes
            .unwrap_or(DEFAULT_FIELD_BYTES)
            .clamp(MIN_FIELD_BYTES, MAX_FIELD_BYTES),
        total_bytes: max_total_bytes
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
        .take(MAX_CANDIDATES)
        .enumerate()
        .map(|(idx, text)| LeanWorkerProofCandidate {
            id: format!("candidate_{}", idx.saturating_add(1)),
            text,
        })
        .collect()
}

fn capped_candidate_rows(req: &TryProofStepRequest) -> Vec<ProofAttemptCandidate> {
    requested_snippets(req)
        .into_iter()
        .enumerate()
        .skip(MAX_CANDIDATES)
        .map(|(idx, text)| ProofAttemptCandidate {
            id: format!("candidate_{}", idx.saturating_add(1)),
            status: "budget_exceeded".to_owned(),
            snippet: crate::projections::RenderedText {
                value: text,
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
            output_truncated: false,
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

fn append_capped_rows(result: ProofAttemptResult, extra_rows: Vec<ProofAttemptCandidate>) -> ProofAttemptResult {
    match result {
        ProofAttemptResult::Ok { result, imports } => ProofAttemptResult::Ok {
            result: append_rows(result, extra_rows),
            imports,
        },
        ProofAttemptResult::MissingImports {
            result,
            imports,
            missing,
        } => ProofAttemptResult::MissingImports {
            result: append_rows(result, extra_rows),
            imports,
            missing,
        },
        ProofAttemptResult::HeaderParseFailed { diagnostics } => ProofAttemptResult::HeaderParseFailed { diagnostics },
        ProofAttemptResult::Unsupported => ProofAttemptResult::Unsupported,
    }
}

fn append_rows(mut envelope: ProofAttemptEnvelope, mut extra_rows: Vec<ProofAttemptCandidate>) -> ProofAttemptEnvelope {
    envelope.candidates.append(&mut extra_rows);
    envelope.candidate_limit = MAX_CANDIDATES as u32;
    envelope.candidates_truncated = envelope.candidates_truncated || envelope.candidates.len() > MAX_CANDIDATES;
    envelope
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
    fn try_proof_step_request_accepts_snippet_list_and_caps_rows() {
        let snippets = (0..10).map(|idx| format!("exact h{idx}")).collect::<Vec<_>>();
        let req = TryProofStepRequest {
            file: PathBuf::from("Demo.lean"),
            declaration: "Demo.closed".to_owned(),
            proof_position: ProofPositionSelector::Default,
            project: None,
            snippet: None,
            snippets,
            max_field_bytes: None,
            max_total_bytes: None,
            heartbeat_limit: None,
        };
        assert_eq!(proof_candidates(&req).len(), MAX_CANDIDATES);
        let capped = capped_candidate_rows(&req);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].status, "budget_exceeded");
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
    fn proof_action_budget_clamps() {
        let low = proof_action_budgets(Some(1), Some(1));
        assert_eq!(low.per_field_bytes, MIN_FIELD_BYTES);
        assert_eq!(low.total_bytes, MIN_TOTAL_BYTES);

        let high = proof_action_budgets(Some(u32::MAX), Some(u32::MAX));
        assert_eq!(high.per_field_bytes, MAX_FIELD_BYTES);
        assert_eq!(high.total_bytes, MAX_TOTAL_BYTES);
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
