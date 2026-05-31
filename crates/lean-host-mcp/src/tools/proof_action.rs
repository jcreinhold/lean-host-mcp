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
use crate::envelope::Response;
use crate::error::Result;
use crate::projections::{
    DeclarationVerificationResult, ElabFailure, ProofAttemptCandidate, ProofAttemptEnvelope, ProofAttemptResult,
    project_declaration_verification, project_proof_attempt,
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

    let request = LeanWorkerProofAttemptRequest {
        source: input.source,
        edit: LeanWorkerProofEditTarget::Declaration {
            name: req.declaration.clone(),
            position: worker_proof_position(req.proof_position.clone()),
        },
        candidates,
        budgets,
    };
    let call = ctx
        .broker
        .attempt_proof(
            hint,
            session_imports(input.imports.clone()),
            input.imports,
            request,
            elab_options(&file_label, req.heartbeat_limit),
        )
        .await?;
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
        response =
            crate::diagnosis::warn_needs_build(response, &crate::diagnosis::IncompleteCause::MissingImports(missing));
    }
    Ok(response)
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
    let call = ctx
        .broker
        .verify_declaration(
            hint,
            session_imports(input.imports.clone()),
            input.imports,
            request,
            elab_options(&file_label, req.heartbeat_limit),
        )
        .await?;
    let mut response =
        Response::ok(project_declaration_verification(call.value), call.freshness).with_runtime(call.runtime);
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
        response = crate::diagnosis::warn_needs_build(response, &cause);
    }
    response = crate::diagnosis::warn_ambiguous(response, &candidates);
    if let Some(warning) = axiom_warning {
        response = response.warn(warning);
    }
    Ok(response)
}

/// Incomplete-build cause for a verification result, if the verdict was
/// computed against an environment that was not fully assembled.
fn verification_incomplete_cause(result: &DeclarationVerificationResult) -> Option<crate::diagnosis::IncompleteCause> {
    use crate::diagnosis::IncompleteCause;
    match result {
        // The worker reports needs_build through the MissingImports outcome,
        // which names the unbuilt modules.
        DeclarationVerificationResult::MissingImports { missing, .. } => {
            Some(IncompleteCause::MissingImports(missing.clone()))
        }
        DeclarationVerificationResult::Ok {
            verification_status, ..
        } if verification_status == crate::diagnosis::NEEDS_BUILD_STATUS => {
            Some(IncompleteCause::MissingImports(Vec::new()))
        }
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
}
