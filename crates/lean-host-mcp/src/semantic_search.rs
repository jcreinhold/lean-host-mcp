//! Private `lean-semantic-search` runtime adapter.
//!
//! The MCP tool surface stays proof-agent shaped. This module hides the
//! downstream command envelopes, feature rows, and retrieval policy: it writes
//! the request that search's driver reads, and turns the driver's reply into
//! candidate declarations with key-free evidence labels.

use std::collections::HashMap;
use std::path::PathBuf;

use lean_semantic_search_contract::{
    DeclarationFeatureRequest, DeclarationKind, Diagnostic, DiagnosticSeverity, DriverReply, DriverRequest, ModuleSpec,
    ProofGoalFeatureRequest,
};
use lean_semantic_search_retrieval::{Anchor, SemanticIndex, retrieve_across};

use crate::error::{Result, ServerError};

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
    pub(crate) module: String,
    pub(crate) kind: DeclarationKind,
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

impl SemanticProofSearchRequest {
    /// The request search's driver reads, with `roots` as its module search
    /// path. The declarations of each candidate module carry this server as
    /// their origin.
    pub(crate) fn into_driver_request(self, roots: Vec<PathBuf>) -> DriverRequest {
        DriverRequest {
            roots,
            goal: self.goal,
            declarations: DeclarationFeatureRequest {
                modules: self
                    .candidate_modules
                    .into_iter()
                    .map(|module| ModuleSpec {
                        module,
                        origin: Some("lean-host-mcp".to_owned()),
                    })
                    .collect(),
                declaration_ids: Vec::new(),
            },
        }
    }
}

/// Check what search's driver printed and rank its declarations against its
/// goal, keeping the best `limit`.
///
/// # Errors
///
/// Returns the contract's refusal when the output is not a reply that this
/// contract reads.
pub(crate) fn rank_driver_reply(stdout: &[u8], limit: usize) -> Result<SemanticProofSearchResult> {
    let reply = DriverReply::parse(stdout).map_err(|error| ServerError::Internal(error.to_string()))?;
    Ok(rank_semantic_rows(&reply, limit))
}

fn rank_semantic_rows(reply: &DriverReply, limit: usize) -> SemanticProofSearchResult {
    let mut diagnostics = Vec::new();
    diagnostics.extend(diagnostic_strings(&reply.goal.diagnostics));
    diagnostics.extend(diagnostic_strings(&reply.declarations.diagnostics));
    let declarations = &reply.declarations.rows;

    let Some(goal) = reply.goal.rows.first() else {
        return SemanticProofSearchResult {
            candidates: Vec::new(),
            diagnostics,
            declaration_rows: declarations.len(),
            goal_rows: 0,
        };
    };
    if declarations.is_empty() {
        return SemanticProofSearchResult {
            candidates: Vec::new(),
            diagnostics,
            declaration_rows: 0,
            goal_rows: reply.goal.rows.len(),
        };
    }

    let rows_by_id = declarations
        .iter()
        .map(|row| (row.declaration_id.as_str(), row))
        .collect::<HashMap<_, _>>();
    let index = SemanticIndex::from_declarations(declarations);
    let anchor = Anchor::from_proof_goal(goal);
    let retrieval = retrieve_across(&[&index], &anchor, limit);
    diagnostics.extend(diagnostic_strings(&retrieval.diagnostics));

    // Retrieval names only the rows it indexed, so each candidate finds its
    // row.
    let candidates = retrieval
        .candidates
        .into_iter()
        .filter_map(|candidate| {
            let row = rows_by_id.get(candidate.declaration_id.as_str())?;
            let rank = i32::try_from(candidate.rank).unwrap_or(i32::MAX);
            let evidence = candidate
                .explanations
                .into_iter()
                .map(|explanation| format!("semantic:{}:{}", explanation.family.label(), explanation.match_count))
                .collect::<Vec<_>>();
            Some(SemanticProofCandidate {
                name: row.name.clone(),
                module: row.module.clone(),
                kind: row.kind,
                // Keep semantic candidates above lexical-only fallback while
                // still allowing proof-agent boosts/penalties to reorder them.
                score: 150_i32.saturating_sub(rank.saturating_mul(4)),
                evidence,
            })
        })
        .collect();

    SemanticProofSearchResult {
        candidates,
        diagnostics,
        declaration_rows: declarations.len(),
        goal_rows: reply.goal.rows.len(),
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

#[cfg(test)]
mod tests {
    use lean_semantic_search_contract::{
        CAPABILITY_SCHEMA_VERSION, CommandResponse, DECLARATION_FEATURE_COMMAND_VERSION, DECLARATION_FEATURES_COMMAND,
        DeclarationFeatureRow, DeclarationKind, DriverReply, Fingerprints, OpaqueFeatureKey,
        PROOF_GOAL_FEATURE_COMMAND_VERSION, PROOF_GOAL_FEATURES_COMMAND, ProofGoalFeatureRow, RoleFeature,
        SEMANTIC_FEATURE_VERSION,
    };

    use super::{SemanticProofSearchResult, rank_driver_reply};

    fn fingerprints(seed: &str) -> Fingerprints {
        Fingerprints {
            statement: OpaqueFeatureKey::new(format!("stmt:{seed}")),
            safe_binder_permutation: OpaqueFeatureKey::new(format!("safe:{seed}")),
            connective_shape: OpaqueFeatureKey::new(format!("conn:{seed}")),
            conclusion_shape: OpaqueFeatureKey::new(format!("concl:{seed}")),
        }
    }

    fn conclusion(key: &OpaqueFeatureKey) -> Vec<RoleFeature> {
        vec![RoleFeature {
            role: "conclusion_const".to_owned(),
            key: key.clone(),
            display: Some("Target.const".to_owned()),
        }]
    }

    fn envelope<Row>(command: &str, command_version: &str, rows: Vec<Row>) -> CommandResponse<Row> {
        CommandResponse {
            schema_version: CAPABILITY_SCHEMA_VERSION.to_owned(),
            command: command.to_owned(),
            command_version: command_version.to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            rows,
            diagnostics: Vec::new(),
        }
    }

    /// A row with the conclusion constant `key` and the given identity.
    fn row(key: &OpaqueFeatureKey, declaration_id: &str, module: &str, name: &str) -> DeclarationFeatureRow {
        DeclarationFeatureRow {
            declaration_id: declaration_id.to_owned(),
            module: module.to_owned(),
            name: name.to_owned(),
            kind: DeclarationKind::Instance,
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("row"),
            role_features: conclusion(key),
            binder_count: 0,
            low_signal_markers: Vec::new(),
            source: None,
        }
    }

    /// A reply whose goal shares the conclusion constant `key` with `row`.
    fn reply(key: &OpaqueFeatureKey, row: DeclarationFeatureRow) -> DriverReply {
        let goal = ProofGoalFeatureRow {
            goal_id: "g".to_owned(),
            feature_version: SEMANTIC_FEATURE_VERSION.to_owned(),
            fingerprints: fingerprints("goal"),
            role_features: conclusion(key),
            low_signal_markers: Vec::new(),
        };
        DriverReply {
            goal: envelope(
                PROOF_GOAL_FEATURES_COMMAND,
                PROOF_GOAL_FEATURE_COMMAND_VERSION,
                vec![goal],
            ),
            declarations: envelope(
                DECLARATION_FEATURES_COMMAND,
                DECLARATION_FEATURE_COMMAND_VERSION,
                vec![row],
            ),
        }
    }

    /// Rank `reply` as the broker does, from the line the driver prints.
    fn rank(reply: &DriverReply) -> Result<SemanticProofSearchResult, String> {
        let stdout = serde_json::to_vec(reply).map_err(|error| error.to_string())?;
        rank_driver_reply(&stdout, 5).map_err(|error| error.to_string())
    }

    #[test]
    fn retrieval_mapping_hides_raw_keys() -> Result<(), String> {
        let key = OpaqueFeatureKey::new("opaque-key-that-must-not-leak");
        let result = rank(&reply(&key, row(&key, "id", "Fixture", "Fixture.target_helper")))?;
        let [candidate] = result.candidates.as_slice() else {
            return Err(format!("expected one candidate, got {}", result.candidates.len()));
        };
        let evidence = candidate.evidence.join(",");
        assert!(evidence.contains("semantic:role_conclusion_const"), "{evidence}");
        assert!(!evidence.contains("opaque-key-that-must-not-leak"), "{evidence}");
        Ok(())
    }

    /// The id stays opaque: an id in no known shape still yields the row's
    /// own module, name and kind.
    #[test]
    fn candidates_take_module_name_and_kind_from_their_row() -> Result<(), String> {
        let key = OpaqueFeatureKey::new("shared-conclusion-key");
        let result = rank(&reply(&key, row(&key, "opaque:7", "KanProofs.Foo", "My.Decl.name")))?;
        let Some(candidate) = result.candidates.first() else {
            return Err("expected a semantic candidate".to_owned());
        };
        assert_eq!(
            (candidate.module.as_str(), candidate.name.as_str(), candidate.kind),
            ("KanProofs.Foo", "My.Decl.name", DeclarationKind::Instance)
        );
        Ok(())
    }

    /// A reply from another contract version fails the search and names the
    /// field that differs.
    #[test]
    fn a_reply_from_another_contract_fails_the_search() {
        let key = OpaqueFeatureKey::new("key");
        let mut reply = reply(&key, row(&key, "id", "M", "M.x"));
        "declaration_features.v1".clone_into(&mut reply.declarations.command_version);
        let error = rank(&reply).err().unwrap_or_default();
        assert!(error.contains("command_version"), "{error}");
    }
}
