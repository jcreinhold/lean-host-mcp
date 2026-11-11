//! Content-free summaries of actual semantic executions, not polling RPCs.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::Value;

use crate::error::{Result, ServerError};
use crate::tools::request_aliases::canonical_kind;
use crate::tools::semantic::SemanticResponse;

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// Only operational metadata is retained: never requests or response bodies.
pub(crate) struct RequestLog {
    id: u64,
    tool: &'static str,
    kind: &'static str,
    project: String,
    started: Instant,
    finished: bool,
}

impl RequestLog {
    pub(crate) fn new(tool: &'static str, kind: Option<&str>, project: Option<&str>) -> Self {
        let log = Self {
            id: NEXT_REQUEST.fetch_add(1, Ordering::Relaxed),
            tool,
            kind: safe_kind(tool, kind),
            project: project_label(project),
            started: Instant::now(),
            finished: false,
        };
        tracing::info!(
            request = log.id, tool = log.tool, kind = log.kind, project = %log.project,
            "Lean request received (preparing or waiting for worker)"
        );
        log
    }

    pub(crate) fn finish(&mut self, result: &Result<SemanticResponse<Value>>) {
        let elapsed_ms = self.started.elapsed().as_millis();
        match result {
            Ok(response) => {
                if !response.trust.project_root.is_empty() {
                    self.project = project_label(Some(&response.trust.project_root));
                }
                let errors = response
                    .errors
                    .iter()
                    .filter(|issue| issue.severity.as_deref() == Some("error"))
                    .count();
                let warnings = response.errors.len().saturating_sub(errors);
                let summary = response.data.as_ref().and_then(|data| match (self.tool, self.kind) {
                    ("lean_verify", "verification") => data.get("summary"),
                    ("lean_trial", "proof_step") => data.get("result").and_then(|result| result.get("summary")),
                    _ => None,
                });
                let count = |key| summary.and_then(|summary| summary.get(key)).and_then(Value::as_u64);
                let diagnostics = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("diagnostics"))
                    .and_then(|data| data.get("summary"));
                let diagnostic_count = |key| diagnostics.and_then(|summary| summary.get(key)).and_then(Value::as_u64);
                let outcome = if response.data.is_none() && errors > 0 {
                    "tool_error"
                } else if !response.errors.is_empty() {
                    "completed_with_issues"
                } else {
                    "completed"
                };
                tracing::info!(
                    request = self.id, tool = self.tool, kind = self.kind, project = %self.project,
                    elapsed_ms, outcome, result_status = result_status(response.data.as_ref()),
                    tool_errors = errors, tool_warnings = warnings,
                    diagnostic_errors = diagnostic_count("errors"), diagnostic_warnings = diagnostic_count("warnings"),
                    verified = count("verified"), failed = count("failed"), needs_build = count("needs_build"),
                    closed = count("closed"), progressed = count("progressed"), timeout = count("timeout"),
                    "Lean response ready"
                );
            }
            Err(error) => {
                // Error Display/Debug can contain source and diagnostic text.
                let outcome = match error {
                    ServerError::WorkerUnavailable(info) if info.reason == "request_cancelled" => "cancelled",
                    ServerError::WorkerUnavailable(_) => "runtime_unavailable",
                    ServerError::IncompatibleWorker { .. } => "incompatible_worker",
                    ServerError::BadProject(_) => "bad_project",
                    ServerError::SessionGone => "session_gone",
                    ServerError::Lean(_) => "lean_error",
                    ServerError::Io(_) => "io_error",
                    ServerError::Internal(_) => "internal_error",
                };
                tracing::warn!(
                    request = self.id, tool = self.tool, kind = self.kind, project = %self.project,
                    elapsed_ms, outcome, "Lean request returned an error"
                );
            }
        }
        self.finished = true;
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        if !self.finished {
            tracing::warn!(
                request = self.id, tool = self.tool, kind = self.kind, project = %self.project,
                elapsed_ms = self.started.elapsed().as_millis(), outcome = "interrupted",
                "Lean request future dropped; worker completion is not confirmed"
            );
        }
    }
}

fn safe_kind(tool: &str, kind: Option<&str>) -> &'static str {
    let kind = kind.map(str::trim).filter(|kind| !kind.is_empty());
    if tool == "lean_verify" {
        return "verification";
    }
    let Some(kind) = kind.or_else(|| (tool == "lean_status").then_some("project")) else {
        return "missing";
    };
    match (tool, canonical_kind(tool, kind)) {
        ("lean_context", "proof_position") => "proof_position",
        ("lean_trial", "proof_step") => "proof_step",
        ("lean_trial", "command") => "command",
        ("lean_lookup", "declaration") => "declaration",
        ("lean_lookup", "declarations") => "declarations",
        ("lean_lookup", "proof_search") => "proof_search",
        ("lean_lookup", "references") => "references",
        ("lean_status", "project") => "project",
        ("lean_status", "file_diagnostics") => "file_diagnostics",
        _ => "invalid",
    }
}

fn result_status(data: Option<&Value>) -> Option<&'static str> {
    match data.and_then(|data| data.get("status")).and_then(Value::as_str) {
        Some("ok") => Some("ok"),
        Some("found") => Some("found"),
        Some("not_found") => Some("not_found"),
        Some("ambiguous") => Some("ambiguous"),
        Some("needs_build") => Some("needs_build"),
        Some("unsupported") => Some("unsupported"),
        Some("missing_imports") => Some("missing_imports"),
        Some("header_parse_failed") => Some("header_parse_failed"),
        _ => None,
    }
}

fn project_label(project: Option<&str>) -> String {
    let Some(project) = project else {
        return "default".to_owned();
    };
    let name = Path::new(project)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    // No full user paths, terminal control sequences, or unbounded caller text.
    name.chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log buffer poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn capture() -> (tracing::subscriber::DefaultGuard, Buffer) {
        capture_with_filter("lean_host_mcp::request_log=info")
    }

    pub(crate) fn capture_with_filter(filter: &str) -> (tracing::subscriber::DefaultGuard, Buffer) {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_writer(move || writer.clone())
            .finish();
        (tracing::subscriber::set_default(subscriber), buffer)
    }

    #[test]
    fn summaries_pair_ids_preserve_negative_verdict_counts_and_omit_content() {
        use crate::tools::semantic::{SemanticIssue, SemanticTrust};
        let (_guard, buffer) = capture();
        let mut log = RequestLog::new("lean_verify", None, Some("/private/proofs"));
        let id = log.id;
        log.finish(&Ok(SemanticResponse {
            data: Some(serde_json::json!({"summary":{"verified":2,"failed":1,"needs_build":0},"proof":"SECRET_PROOF","diagnostics":{"summary":{"errors":3,"warnings":1},"diagnostics":["SECRET_DIAGNOSTIC"]}})),
            errors: vec![SemanticIssue {
                code: "SECRET_CODE".to_owned(),
                message: "SECRET_DIAGNOSTIC".to_owned(),
                severity: Some("warning".to_owned()),
                next_action: None,
                retryable: None,
                details: None,
            }],
            trust: SemanticTrust {
                project_root: "/private/proofs".to_owned(),
                session_id: "SECRET_SESSION".to_owned(),
                lean_toolchain: String::new(),
                artifacts: Vec::new(),
            },
            telemetry: None,
        }));
        drop(log);
        let output = buffer.text();
        assert_eq!(output.matches(&format!("request={id} ")).count(), 2);
        assert!(output.contains("verified=2 failed=1 needs_build=0"));
        assert!(output.contains("diagnostic_errors=3 diagnostic_warnings=1"));
        assert!(output.contains(r#"outcome="completed_with_issues""#));
        assert!(!output.contains("SECRET"));
        assert!(!output.contains("/private"));
        assert!(!output.contains("future dropped"));
    }

    #[test]
    fn typed_proof_step_variants_report_nested_summary_without_proof_content() {
        use crate::projections::{
            ElabFailure, ProofAttemptCandidate, ProofAttemptEnvelope, ProofAttemptResult, ProofAttemptSummary,
            RenderedText,
        };
        use crate::tools::semantic::SemanticTrust;
        let candidates = ["closed", "progressed", "timeout"]
            .map(|status| ProofAttemptCandidate {
                id: "SECRET_ID".to_owned(),
                status: status.to_owned(),
                snippet: RenderedText {
                    value: "SECRET_TACTIC".to_owned(),
                    truncated: false,
                },
                diagnostics: ElabFailure::default(),
                downstream_diagnostics: ElabFailure::default(),
                post_closure_diagnostics: ElabFailure::default(),
                goals: Vec::new(),
                declaration: None,
                proof_position: None,
                output_truncated: false,
            })
            .to_vec();
        let mut envelope = ProofAttemptEnvelope {
            candidates,
            candidate_limit: 3,
            candidates_truncated: false,
            entry_goals: Vec::new(),
            locals: Vec::new(),
            summary: ProofAttemptSummary {
                requested_candidates: 0,
                returned_candidates: 0,
                candidate_limit: 3,
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
        envelope.refresh_summary(3);
        for result in [
            ProofAttemptResult::Ok {
                result: envelope.clone(),
                imports: vec!["SECRET_IMPORT".to_owned()],
            },
            ProofAttemptResult::MissingImports {
                result: envelope.clone(),
                imports: Vec::new(),
                missing: vec!["SECRET_IMPORT".to_owned()],
            },
        ] {
            let (_guard, buffer) = capture();
            let mut log = RequestLog::new("lean_trial", Some("proof_step"), None);
            log.finish(&Ok(SemanticResponse {
                data: Some(serde_json::to_value(result).unwrap()),
                errors: Vec::new(),
                telemetry: None,
                trust: SemanticTrust {
                    project_root: String::new(),
                    session_id: String::new(),
                    lean_toolchain: String::new(),
                    artifacts: Vec::new(),
                },
            }));
            let output = buffer.text();
            assert!(output.contains("closed=1 progressed=1 timeout=1"));
            assert!(!output.contains("SECRET"));
        }
    }

    #[test]
    fn typed_negative_result_statuses_are_informative_without_echoing_names() {
        use crate::projections::{DeclarationInspectionResult, ElabFailure, ProofAttemptResult};
        use crate::tools::semantic::SemanticTrust;
        let cases = [
            (
                serde_json::to_value(DeclarationInspectionResult::NotFound {
                    name: Some("SECRET_DECLARATION".to_owned()),
                })
                .unwrap(),
                "not_found",
            ),
            (
                serde_json::to_value(ProofAttemptResult::Unsupported).unwrap(),
                "unsupported",
            ),
            (
                serde_json::to_value(ProofAttemptResult::HeaderParseFailed {
                    diagnostics: ElabFailure::default(),
                })
                .unwrap(),
                "header_parse_failed",
            ),
        ];
        for (data, expected) in cases {
            let (_guard, buffer) = capture();
            let mut log = RequestLog::new("lean_lookup", Some("declaration"), None);
            log.finish(&Ok(SemanticResponse {
                data: Some(data),
                errors: Vec::new(),
                telemetry: None,
                trust: SemanticTrust {
                    project_root: String::new(),
                    session_id: String::new(),
                    lean_toolchain: String::new(),
                    artifacts: Vec::new(),
                },
            }));
            let output = buffer.text();
            assert!(output.contains(&format!(r#"result_status="{expected}""#)));
            assert!(!output.contains("SECRET"));
        }
        assert_eq!(
            result_status(Some(&serde_json::json!({"status":"SECRET_UNKNOWN_STATUS"}))),
            None
        );
    }

    #[test]
    fn errors_and_dropped_futures_do_not_claim_success_or_worker_termination() {
        let (_guard, buffer) = capture();
        let mut log = RequestLog::new("lean_trial", Some("command"), None);
        log.finish(&Err(ServerError::Lean("SECRET_DIAGNOSTIC".to_owned())));
        drop(log);
        drop(RequestLog::new("lean_trial", Some("command"), None));
        let output = buffer.text();
        assert!(output.contains(r#"outcome="lean_error""#));
        assert!(output.contains("worker completion is not confirmed"));
        assert_eq!(output.matches("future dropped").count(), 1);
        assert!(!output.contains("SECRET"));
        assert!(!output.contains("response ready"));
    }

    #[test]
    fn metadata_is_bounded_and_does_not_echo_unknown_kinds_or_paths() {
        assert_eq!(safe_kind("lean_trial", Some("secret proof body\n")), "invalid");
        assert_eq!(safe_kind("lean_trial", Some(" tactics ")), "proof_step");
        assert_eq!(safe_kind("lean_status", None), "project");
        assert_eq!(safe_kind("lean_verify", None), "verification");
        assert_eq!(project_label(Some("/Users/private/Code/proofs")), "proofs");
        assert_eq!(project_label(Some("/tmp/proof\n\x1b[31m")), "proof___31m");
        assert_eq!(project_label(Some(&"x".repeat(1000))).len(), 80);
    }
}
