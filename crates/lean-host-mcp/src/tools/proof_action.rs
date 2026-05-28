//! Non-mutating proof action tools.
//!
//! `try_proof_step` and `verify_declaration` read a Lean file, send its
//! contents to the worker as an in-memory overlay, and return structured
//! proof/verification outcomes. They never write source files.

// Tool handlers consume request structs so owned strings can cross the
// worker-actor channel without extra lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationTarget, LeanWorkerElabOptions,
    LeanWorkerOutputBudgets, LeanWorkerProofAttemptRequest, LeanWorkerProofCandidate, LeanWorkerProofEditTarget,
    LeanWorkerSorryPolicy,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::project::ProjectWorkClass;
use crate::projections::{
    DeclarationVerificationResult, ElabFailure, ProofAttemptCandidate, ProofAttemptEnvelope, ProofAttemptResult,
    project_declaration_verification, project_proof_attempt,
};
use crate::tools::position::{ProofPositionSelector, worker_proof_position};
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
    ctx.broker
        .with_project(hint, move |project| async move {
            let input = read_query_file(project.canonical_root(), &req.file)?;
            let file_label = input.resolved.to_string_lossy().into_owned();
            let freshness = project.freshness(&input.imports);
            let budgets = proof_action_budgets(req.max_field_bytes, req.max_total_bytes);
            let candidates = proof_candidates(&req);
            let extra_rows = capped_candidate_rows(&req);

            if candidates.is_empty() {
                return Ok(Response::ok(
                    ProofAttemptResult::Ok {
                        result: ProofAttemptEnvelope {
                            candidates: extra_rows,
                            candidate_limit: MAX_CANDIDATES as u32,
                            candidates_truncated: false,
                        },
                        imports: input.imports,
                    },
                    freshness,
                )
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
            let call = project
                .call(ProjectWorkClass::Semantic, input.imports.clone(), move |cap| {
                    let mut session =
                        cap.open_session_with_imports(session_imports(input.imports.clone()), None, None)?;
                    session.attempt_proof(&request, &elab_options(&file_label, req.heartbeat_limit), None, None)
                })
                .await?;
            let mut response = Response::ok(
                append_capped_rows(project_proof_attempt(call.value), extra_rows),
                freshness,
            )
            .with_runtime(call.runtime);
            response
                .next_actions
                .push("source file was not modified; apply the chosen snippet manually if desired".to_owned());
            Ok(response)
        })
        .await
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
    ctx.broker
        .with_project(hint, move |project| async move {
            let input = read_query_file(project.canonical_root(), &req.file)?;
            let file_label = input.resolved.to_string_lossy().into_owned();
            let freshness = project.freshness(&input.imports);
            let budgets = proof_action_budgets(req.max_field_bytes, req.max_total_bytes);
            if req.declaration.trim().is_empty() {
                return Ok(Response::ok(DeclarationVerificationResult::Unsupported, freshness)
                    .warn("verify_declaration requires `declaration`"));
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
            let call = project
                .call(ProjectWorkClass::Semantic, input.imports.clone(), move |cap| {
                    let mut session =
                        cap.open_session_with_imports(session_imports(input.imports.clone()), None, None)?;
                    session.verify_declaration(&request, &elab_options(&file_label, req.heartbeat_limit), None, None)
                })
                .await?;
            let mut response =
                Response::ok(project_declaration_verification(call.value), freshness).with_runtime(call.runtime);
            response
                .next_actions
                .push("source file was not modified by verification".to_owned());
            Ok(response)
        })
        .await
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
