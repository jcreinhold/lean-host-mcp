//! Private `lean-semantic-search` runtime adapter.
//!
//! The MCP tool surface stays proof-agent shaped. This module hides the
//! downstream capability export names, command envelopes, feature rows, and
//! retrieval policy behind one operation that returns candidate declarations
//! plus key-free evidence labels.

use std::collections::HashMap;

use lean_rs_worker_parent::{LeanWorkerJsonCommand, LeanWorkerSession};
use lean_semantic_search_contract::{
    CAPABILITY_SCHEMA_VERSION, CommandResponse, DECLARATION_FEATURE_COMMAND_VERSION, DeclarationFeatureRow, Diagnostic,
    DiagnosticSeverity, ModuleSpec, PROOF_GOAL_FEATURE_COMMAND_VERSION, ProofGoalFeatureRequest, ProofGoalFeatureRow,
    SEMANTIC_FEATURE_VERSION,
};
use lean_semantic_search_retrieval::{Anchor, SemanticIndex, retrieve_across};
use serde::Serialize;

use crate::error::{Result, ServerError};
use crate::projections::SourceRange;

/// Source-backed semantic proof-search request built by `search_for_proof`.
#[derive(Debug, Clone)]
pub(crate) struct SemanticProofSearchRequest {
    pub(crate) goal: ProofGoalFeatureRequest,
    pub(crate) candidate_modules: Vec<String>,
    pub(crate) limit: usize,
}

/// Candidate declaration returned after storage-neutral semantic retrieval.
#[derive(Debug, Clone)]
pub(crate) struct SemanticProofCandidate {
    pub(crate) name: String,
    pub(crate) module: Option<String>,
    pub(crate) source: Option<SourceRange>,
    pub(crate) score: i32,
    pub(crate) evidence: Vec<String>,
}

/// Semantic proof-search result, with public-safe diagnostic strings.
#[derive(Debug, Clone)]
pub(crate) struct SemanticProofSearchResult {
    pub(crate) candidates: Vec<SemanticProofCandidate>,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) declaration_rows: usize,
    pub(crate) goal_rows: usize,
}

type DeclarationResponse = CommandResponse<DeclarationFeatureRow>;
type ProofGoalResponse = CommandResponse<ProofGoalFeatureRow>;

#[derive(Debug, Serialize)]
struct DeclarationFeatureCommandRequest {
    modules: Vec<ModuleSpec>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    declaration_ids: Vec<String>,
}

/// Run source-backed semantic proof search in one manifest-backed worker session.
///
/// # Errors
///
/// Returns [`ServerError::Lean`] for worker command failures and
/// [`ServerError::Internal`] for malformed semantic-search envelopes.
pub(crate) fn run_semantic_proof_search(
    session: &mut LeanWorkerSession<'_>,
    request: &SemanticProofSearchRequest,
) -> Result<SemanticProofSearchResult> {
    let goal_command = LeanWorkerJsonCommand::<ProofGoalFeatureRequest, ProofGoalResponse>::new(
        lean_semantic_search_capability::PROOF_GOAL_FEATURES_EXPORT,
    );
    let declaration_command = LeanWorkerJsonCommand::<DeclarationFeatureCommandRequest, DeclarationResponse>::new(
        lean_semantic_search_capability::DECLARATION_FEATURES_EXPORT,
    );

    let goal_response = session
        .run_json_command(&goal_command, &request.goal, None, None)
        .map_err(crate::error::map_worker_err)?;
    validate_response(
        &goal_response,
        lean_semantic_search_capability::PROOF_GOAL_FEATURES_COMMAND,
        PROOF_GOAL_FEATURE_COMMAND_VERSION,
    )?;

    let declaration_request = DeclarationFeatureCommandRequest {
        modules: request
            .candidate_modules
            .iter()
            .map(|module| ModuleSpec {
                module: module.clone(),
                origin: Some("lean-host-mcp".to_owned()),
                source_root: None,
            })
            .collect(),
        declaration_ids: Vec::new(),
    };
    let declaration_response = session
        .run_json_command(&declaration_command, &declaration_request, None, None)
        .map_err(crate::error::map_worker_err)?;
    validate_response(
        &declaration_response,
        lean_semantic_search_capability::DECLARATION_FEATURES_COMMAND,
        DECLARATION_FEATURE_COMMAND_VERSION,
    )?;

    Ok(rank_semantic_rows(&goal_response, &declaration_response, request.limit))
}

fn validate_response<Row>(response: &CommandResponse<Row>, command: &str, command_version: &str) -> Result<()> {
    if response.schema_version != CAPABILITY_SCHEMA_VERSION {
        return Err(ServerError::Internal(format!(
            "semantic search returned schema version {}, expected {}",
            response.schema_version, CAPABILITY_SCHEMA_VERSION
        )));
    }
    if response.command != command {
        return Err(ServerError::Internal(format!(
            "semantic search returned command {}, expected {}",
            response.command, command
        )));
    }
    if response.command_version != command_version {
        return Err(ServerError::Internal(format!(
            "semantic search returned command version {}, expected {}",
            response.command_version, command_version
        )));
    }
    if response.feature_version != SEMANTIC_FEATURE_VERSION {
        return Err(ServerError::Internal(format!(
            "semantic search returned feature version {}, expected {}",
            response.feature_version, SEMANTIC_FEATURE_VERSION
        )));
    }
    Ok(())
}

fn rank_semantic_rows(
    goal_response: &ProofGoalResponse,
    declaration_response: &DeclarationResponse,
    limit: usize,
) -> SemanticProofSearchResult {
    let mut diagnostics = Vec::new();
    diagnostics.extend(diagnostic_strings(&goal_response.diagnostics));
    diagnostics.extend(diagnostic_strings(&declaration_response.diagnostics));

    let Some(goal) = goal_response.rows.first() else {
        return SemanticProofSearchResult {
            candidates: Vec::new(),
            diagnostics,
            declaration_rows: declaration_response.rows.len(),
            goal_rows: 0,
        };
    };
    if declaration_response.rows.is_empty() {
        return SemanticProofSearchResult {
            candidates: Vec::new(),
            diagnostics,
            declaration_rows: 0,
            goal_rows: goal_response.rows.len(),
        };
    }

    let source_by_id = declaration_response
        .rows
        .iter()
        .map(|row| (row.declaration_id.clone(), row.source.map(source_range)))
        .collect::<HashMap<_, _>>();
    let index = SemanticIndex::from_declarations(&declaration_response.rows);
    let anchor = Anchor::from_proof_goal(goal);
    let retrieval = retrieve_across(&[&index], &anchor, limit);
    diagnostics.extend(diagnostic_strings(&retrieval.diagnostics));

    let candidates = retrieval
        .candidates
        .into_iter()
        .map(|candidate| {
            let rank = i32::try_from(candidate.rank).unwrap_or(i32::MAX);
            let evidence = candidate
                .explanations
                .into_iter()
                .map(|explanation| format!("semantic:{}:{}", explanation.family.label(), explanation.match_count))
                .collect::<Vec<_>>();
            let source = source_by_id.get(&candidate.declaration_id).cloned().flatten();
            let (module, name) = split_declaration_id(&candidate.declaration_id);
            SemanticProofCandidate {
                name,
                module,
                source,
                // Keep semantic candidates above lexical-only fallback while
                // still allowing proof-agent boosts/penalties to reorder them.
                score: 150_i32.saturating_sub(rank.saturating_mul(4)),
                evidence,
            }
        })
        .collect();

    SemanticProofSearchResult {
        candidates,
        diagnostics,
        declaration_rows: declaration_response.rows.len(),
        goal_rows: goal_response.rows.len(),
    }
}

fn diagnostic_strings(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .filter(|diagnostic| !matches!(diagnostic.severity, DiagnosticSeverity::Pass))
        .map(|diagnostic| {
            format!(
                "{}:{}:{}",
                severity_label(diagnostic.severity),
                diagnostic.code,
                diagnostic.message
            )
        })
        .collect()
}

const fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Pass => "pass",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Error => "error",
    }
}

/// Split a downstream `origin:module:declName` declaration id into its module
/// and the clean Lean declaration name.
///
/// The extractor builds ids as `s!"{origin}:{module}:{declName}"`, and none of
/// the three parts contains a colon (Lean names use `.`, the origin is a fixed
/// label). An id that does not match that shape is returned whole as the name,
/// so a future format change degrades to a still-usable name rather than an
/// error.
fn split_declaration_id(id: &str) -> (Option<String>, String) {
    let mut parts = id.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(_origin), Some(module), Some(decl)) => (Some(module.to_owned()), decl.to_owned()),
        _ => (None, id.to_owned()),
    }
}

fn source_range(span: lean_semantic_search_contract::SourceSpan) -> SourceRange {
    SourceRange {
        file: String::new(),
        start_line: span.start.line,
        start_column: span.start.column,
        end_line: span.end.line,
        end_column: span.end.column,
    }
}

#[cfg(test)]
mod tests {
    use lean_semantic_search_contract::{
        CAPABILITY_SCHEMA_VERSION, CommandResponse, DECLARATION_FEATURE_COMMAND_VERSION, Diagnostic, Fingerprints,
        OpaqueFeatureKey, PROOF_GOAL_FEATURE_COMMAND_VERSION, RoleFeature, SEMANTIC_FEATURE_VERSION, SourcePosition,
        SourceSpan,
    };

    use super::{rank_semantic_rows, validate_response};

    fn fingerprints(seed: &str) -> Fingerprints {
        Fingerprints {
            statement: OpaqueFeatureKey::new(format!("stmt:{seed}")),
            safe_binder_permutation: OpaqueFeatureKey::new(format!("safe:{seed}")),
            connective_shape: OpaqueFeatureKey::new(format!("conn:{seed}")),
            conclusion_shape: OpaqueFeatureKey::new(format!("concl:{seed}")),
        }
    }

    #[test]
    fn validation_rejects_wrong_command_version() -> Result<(), String> {
        let response = CommandResponse::<lean_semantic_search_contract::DeclarationFeatureRow> {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: "declaration_features".to_owned(),
            command_version: "old".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows: Vec::new(),
            diagnostics: Vec::new(),
        };
        let err = match validate_response(&response, "declaration_features", DECLARATION_FEATURE_COMMAND_VERSION) {
            Ok(()) => return Err("wrong command version must fail".to_owned()),
            Err(err) => err,
        };
        assert!(err.to_string().contains("command version"));
        Ok(())
    }

    #[test]
    fn retrieval_mapping_hides_raw_keys() -> Result<(), String> {
        let key = OpaqueFeatureKey::new("opaque-key-that-must-not-leak");
        let goal = lean_semantic_search_contract::ProofGoalFeatureRow {
            goal_id: "g".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("goal"),
            role_features: vec![RoleFeature {
                role: "conclusion_const".to_owned(),
                key: key.clone(),
                display: Some("Target.const".to_owned()),
            }],
            low_signal_markers: Vec::new(),
        };
        let row = lean_semantic_search_contract::DeclarationFeatureRow {
            declaration_id: "Fixture.target_helper".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("row"),
            role_features: vec![RoleFeature {
                role: "conclusion_const".to_owned(),
                key,
                display: Some("Target.const".to_owned()),
            }],
            binder_count: 0,
            low_signal_markers: Vec::new(),
            source: Some(SourceSpan {
                start: SourcePosition { line: 2, column: 3 },
                end: SourcePosition { line: 2, column: 30 },
            }),
        };
        let goal_response = CommandResponse {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: "proof_goal_features".to_owned(),
            command_version: PROOF_GOAL_FEATURE_COMMAND_VERSION.to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows: vec![goal],
            diagnostics: Vec::<Diagnostic>::new(),
        };
        let declaration_response = CommandResponse {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: "declaration_features".to_owned(),
            command_version: DECLARATION_FEATURE_COMMAND_VERSION.to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows: vec![row],
            diagnostics: Vec::<Diagnostic>::new(),
        };
        let result = rank_semantic_rows(&goal_response, &declaration_response, 5);
        assert_eq!(result.candidates.len(), 1);
        let Some(candidate) = result.candidates.first() else {
            return Err("expected a semantic candidate".to_owned());
        };
        let evidence = candidate.evidence.join(",");
        assert_eq!(candidate.name, "Fixture.target_helper");
        // A colonless id is not in `origin:module:decl` shape, so it stays whole
        // as the name and carries no module.
        assert!(candidate.module.is_none());
        assert!(evidence.contains("semantic:role_conclusion_const"));
        assert!(!evidence.contains("opaque-key-that-must-not-leak"));
        assert!(candidate.source.is_some());
        Ok(())
    }

    #[test]
    fn candidate_name_strips_origin_module_prefix() -> Result<(), String> {
        let key = OpaqueFeatureKey::new("shared-conclusion-key");
        let goal = lean_semantic_search_contract::ProofGoalFeatureRow {
            goal_id: "g".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("goal"),
            role_features: vec![RoleFeature {
                role: "conclusion_const".to_owned(),
                key: key.clone(),
                display: Some("Target.const".to_owned()),
            }],
            low_signal_markers: Vec::new(),
        };
        // The downstream extractor emits ids as `origin:module:declName`.
        let row = lean_semantic_search_contract::DeclarationFeatureRow {
            declaration_id: "lean-host-mcp:KanProofs.Foo:My.Decl.name".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("row"),
            role_features: vec![RoleFeature {
                role: "conclusion_const".to_owned(),
                key,
                display: Some("Target.const".to_owned()),
            }],
            binder_count: 0,
            low_signal_markers: Vec::new(),
            source: None,
        };
        let goal_response = CommandResponse {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: "proof_goal_features".to_owned(),
            command_version: PROOF_GOAL_FEATURE_COMMAND_VERSION.to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows: vec![goal],
            diagnostics: Vec::<Diagnostic>::new(),
        };
        let declaration_response = CommandResponse {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: "declaration_features".to_owned(),
            command_version: DECLARATION_FEATURE_COMMAND_VERSION.to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows: vec![row],
            diagnostics: Vec::<Diagnostic>::new(),
        };
        let result = rank_semantic_rows(&goal_response, &declaration_response, 5);
        let Some(candidate) = result.candidates.first() else {
            return Err("expected a semantic candidate".to_owned());
        };
        // The public name is the clean Lean name; the origin/module prefix is
        // lifted into `module` rather than leaking into `name`.
        assert_eq!(candidate.name, "My.Decl.name");
        assert_eq!(candidate.module.as_deref(), Some("KanProofs.Foo"));
        assert!(!candidate.name.contains("lean-host-mcp"));
        Ok(())
    }
}
