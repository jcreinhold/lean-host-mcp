//! Public semantic MCP facade.
//!
//! The implementation modules keep the old, narrow operation names because
//! they are useful internal building blocks. This module is the public boundary:
//! five semantic tools, each with a small `kind` namespace, and one response
//! shape (`data`, `errors`, `trust`).

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::broker::{BrokerConfigSnapshot, ProjectHint};
use crate::envelope::{FreshnessIdentity, Response, RuntimeFailure};
use crate::error::{Result, ServerError, WorkerUnavailable};
use crate::tools::{ResponseCarrier, TelemetryVerbosity, ToolContext};
use crate::trust::{ArtifactKind, ArtifactTrust, TrustStatus};

use super::changed_coverage::{self, ChangedCoverageRequest};
use super::declaration::{self, InspectDeclarationRequest};
use super::declaration_inventory::{self, DeclarationInventoryRequest};
use super::position::{self, CommandTrialRequest, FileDiagnosticsRequest, FindReferencesRequest, ProofStateRequest};
use super::proof_action::{self, LeanVerifyRequest, TryProofStepRequest};
use super::proof_search::{self, SearchForProofRequest};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SemanticToolRequest {
    /// Mode within this semantic tool family.
    #[serde(default)]
    pub kind: Option<String>,
    /// Mode-specific fields. Unknown fields are passed to the selected mode's
    /// typed request decoder.
    #[serde(flatten)]
    pub args: BTreeMap<String, Value>,
}

impl SemanticToolRequest {
    fn kind(&self) -> Option<&str> {
        self.kind.as_deref().map(str::trim).filter(|kind| !kind.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SemanticTrust {
    pub project_root: String,
    pub session_id: String,
    pub lean_toolchain: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactTrust>,
}

impl SemanticTrust {
    fn unknown() -> Self {
        Self {
            project_root: String::new(),
            session_id: "request-invalid".to_owned(),
            lean_toolchain: String::new(),
            artifacts: Vec::new(),
        }
    }

    fn from_parts(freshness: FreshnessIdentity, artifacts: Vec<ArtifactTrust>) -> Self {
        Self {
            project_root: freshness.project_root,
            session_id: freshness.session_id,
            lean_toolchain: freshness.lean_toolchain,
            artifacts,
        }
    }
}

impl From<FreshnessIdentity> for SemanticTrust {
    fn from(freshness: FreshnessIdentity) -> Self {
        Self::from_parts(freshness, Vec::new())
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SemanticIssue {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SemanticResponse<T>
where
    T: Serialize + JsonSchema,
{
    pub data: Option<T>,
    pub errors: Vec<SemanticIssue>,
    pub trust: SemanticTrust,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LeanStatusData {
    pub kind: String,
    pub project_root: String,
    pub lean_toolchain: String,
    pub include: Vec<String>,
    pub worker_opened: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<WorkerStatusData>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactTrust>,
    pub broker: BrokerConfigSnapshot,
    pub output: OutputStatus,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkerStatusData {
    pub opened: bool,
    pub status: TrustStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OutputStatus {
    pub response_carrier: String,
    pub telemetry_verbosity: String,
}

#[derive(Debug, Clone, Deserialize)]
struct StatusRequest {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    include: Vec<StatusInclude>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StatusInclude {
    Toolchain,
    Worker,
    Artifacts,
}

impl StatusInclude {
    fn as_str(self) -> &'static str {
        match self {
            Self::Toolchain => "toolchain",
            Self::Worker => "worker",
            Self::Artifacts => "artifacts",
        }
    }
}

/// Proof/term context tool. Initial public mode: `proof_position`.
///
/// # Errors
///
/// Returns infrastructure failures only; invalid semantic modes are returned as
/// structured semantic errors.
pub async fn lean_context(ctx: &ToolContext, req: SemanticToolRequest) -> Result<SemanticResponse<Value>> {
    match req.kind() {
        Some("proof_position") => {
            let request = match decode::<ProofStateRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(position::proof_state(ctx, request).await?, ctx.config.verbosity)
        }
        Some(kind) => Ok(invalid_kind("lean_context", kind, &["proof_position"])),
        None => Ok(missing_kind("lean_context", &["proof_position"])),
    }
}

/// Non-mutating Lean experiments.
///
/// # Errors
///
/// Returns infrastructure failures only; invalid semantic modes are returned as
/// structured semantic errors.
pub async fn lean_trial(ctx: &ToolContext, req: SemanticToolRequest) -> Result<SemanticResponse<Value>> {
    match req.kind() {
        Some("proof_step") => {
            let request = match decode::<TryProofStepRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(proof_action::try_proof_step(ctx, request).await?, ctx.config.verbosity)
        }
        Some("command") => {
            let request = match decode::<CommandTrialRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(position::command_trial(ctx, request).await?, ctx.config.verbosity)
        }
        Some(kind) => Ok(invalid_kind("lean_trial", kind, &["proof_step", "command"])),
        None => Ok(missing_kind("lean_trial", &["proof_step", "command"])),
    }
}

/// Declaration verification for explicit, file-wide, and module-wide target groups.
///
/// # Errors
///
/// Returns infrastructure failures only; invalid semantic modes are returned as
/// structured semantic errors.
pub async fn lean_verify(ctx: &ToolContext, req: SemanticToolRequest) -> Result<SemanticResponse<Value>> {
    match req.kind() {
        None => {
            let request = match decode::<LeanVerifyRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            lean_verify_targets(ctx, request).await
        }
        Some(kind) => Ok(invalid_kind("lean_verify", kind, &[])),
    }
}

/// Typed declaration-verification entry point for the public MCP handler.
///
/// Unlike the kind-dispatched semantic families, `lean_verify` has no `kind`
/// namespace: its top-level request is the target-group batch itself.
///
/// # Errors
///
/// Returns infrastructure failures only; per-declaration Lean failures remain
/// structured semantic data.
pub async fn lean_verify_targets(ctx: &ToolContext, request: LeanVerifyRequest) -> Result<SemanticResponse<Value>> {
    from_tool_response(proof_action::verify_targets(ctx, request).await?, ctx.config.verbosity)
}

/// Semantic lookup and discovery.
///
/// Initial public modes: `declaration`, `declarations`, `changed_coverage`,
/// `proof_search`, and `references`.
///
/// # Errors
///
/// Returns infrastructure failures only; invalid semantic modes are returned as
/// structured semantic errors.
pub async fn lean_lookup(ctx: &ToolContext, req: SemanticToolRequest) -> Result<SemanticResponse<Value>> {
    match req.kind() {
        Some("declaration") => {
            let request = match decode::<InspectDeclarationRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(
                declaration::inspect_declaration(ctx, request).await?,
                ctx.config.verbosity,
            )
        }
        Some("declarations") => {
            let request = match decode::<DeclarationInventoryRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(
                declaration_inventory::declaration_inventory(ctx, request).await?,
                ctx.config.verbosity,
            )
        }
        Some("changed_coverage") => {
            let request = match decode::<ChangedCoverageRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(
                changed_coverage::changed_coverage(ctx, request).await?,
                ctx.config.verbosity,
            )
        }
        Some("proof_search") => {
            let request = match decode::<SearchForProofRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(
                proof_search::search_for_proof(ctx, request).await?,
                ctx.config.verbosity,
            )
        }
        Some("references") => {
            let request = match decode::<FindReferencesRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(position::find_references(ctx, request).await?, ctx.config.verbosity)
        }
        Some(kind) => Ok(invalid_kind(
            "lean_lookup",
            kind,
            &[
                "declaration",
                "declarations",
                "changed_coverage",
                "proof_search",
                "references",
            ],
        )),
        None => Ok(missing_kind(
            "lean_lookup",
            &[
                "declaration",
                "declarations",
                "changed_coverage",
                "proof_search",
                "references",
            ],
        )),
    }
}

/// Cheap project/toolchain/config status. Does not open a worker.
///
/// # Errors
///
/// Returns Lake-project resolution failures. Invalid semantic modes are
/// returned as structured semantic errors.
pub async fn lean_status(ctx: &ToolContext, req: SemanticToolRequest) -> Result<SemanticResponse<Value>> {
    let kind = req.kind().unwrap_or("project").to_owned();
    match kind.as_str() {
        "project" => {
            let request = match decode::<StatusRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            let hint = ProjectHint::from_request(request.project);
            let identity = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
            let project_root = identity.freshness.project_root.clone();
            let includes = status_includes(&request.include);
            let mut artifacts = Vec::new();
            let worker = includes.contains(&StatusInclude::Worker).then(|| WorkerStatusData {
                opened: false,
                status: TrustStatus::NotApplicable,
                detail: "lean_status does not open a worker".to_owned(),
            });
            if worker.is_some() {
                artifacts.push(ArtifactTrust::worker_toolchain_not_applicable(
                    "lean_status did not open a worker to inspect runtime generation",
                ));
            }
            if includes.contains(&StatusInclude::Artifacts) {
                artifacts.extend(status_artifact_facts(Path::new(&project_root)));
            }
            let mut trust = SemanticTrust::from(Response::<()>::ok((), identity.freshness).freshness);
            trust.artifacts.clone_from(&artifacts);
            let data = LeanStatusData {
                kind: "project".to_owned(),
                project_root: trust.project_root.clone(),
                lean_toolchain: trust.lean_toolchain.clone(),
                include: includes
                    .iter()
                    .copied()
                    .map(StatusInclude::as_str)
                    .map(str::to_owned)
                    .collect(),
                worker_opened: false,
                worker,
                artifacts,
                broker: ctx.broker.config_snapshot(),
                output: OutputStatus {
                    response_carrier: response_carrier_name(ctx.config.carrier).to_owned(),
                    telemetry_verbosity: telemetry_verbosity_name(ctx.config.verbosity).to_owned(),
                },
            };
            Ok(SemanticResponse {
                data: Some(serde_json::to_value(data).map_err(|err| ServerError::Internal(err.to_string()))?),
                errors: Vec::new(),
                trust,
            })
        }
        "file_diagnostics" => {
            let request = match decode::<FileDiagnosticsRequest>(req) {
                Ok(request) => request,
                Err(response) => return Ok(*response),
            };
            from_tool_response(position::file_diagnostics(ctx, request).await?, ctx.config.verbosity)
        }
        other => Ok(invalid_kind("lean_status", other, &["project", "file_diagnostics"])),
    }
}

pub(crate) fn from_worker_unavailable(info: &WorkerUnavailable) -> SemanticResponse<Value> {
    let mut old = Response::<Value>::runtime_unavailable(info.failure(), info.freshness(), info.runtime.clone())
        .with_trust_artifact(ArtifactTrust::worker_toolchain_unknown(
            "worker runtime was unavailable for this request",
        ));
    old.drain_advisories();
    from_runtime_response(old)
}

fn from_tool_response<T>(mut response: Response<T>, verbosity: TelemetryVerbosity) -> Result<SemanticResponse<Value>>
where
    T: Serialize + JsonSchema,
{
    response.drain_advisories();
    if !verbosity.is_full() {
        response.drop_telemetry();
    }
    let data = response
        .result
        .map(serde_json::to_value)
        .transpose()
        .map_err(|err| ServerError::Internal(err.to_string()))?;
    let trust = SemanticTrust::from_parts(response.freshness, response.trust_artifacts);
    let errors = semantic_issues(response.runtime_error, response.warnings, response.next_actions);
    Ok(SemanticResponse { data, errors, trust })
}

fn from_runtime_response(response: Response<Value>) -> SemanticResponse<Value> {
    let trust = SemanticTrust::from_parts(response.freshness, response.trust_artifacts);
    let errors = semantic_issues(response.runtime_error, response.warnings, response.next_actions);
    SemanticResponse {
        data: response.result,
        errors,
        trust,
    }
}

fn status_includes(include: &[StatusInclude]) -> Vec<StatusInclude> {
    if include.is_empty() {
        vec![
            StatusInclude::Toolchain,
            StatusInclude::Worker,
            StatusInclude::Artifacts,
        ]
    } else {
        include.to_vec()
    }
}

fn status_artifact_facts(root: &Path) -> Vec<ArtifactTrust> {
    let build_tree = root.join(".lake/build/lib/lean");
    if build_tree.is_dir() {
        let path = build_tree.to_string_lossy().into_owned();
        vec![
            ArtifactTrust::build_tree_unknown(path.clone(), ArtifactKind::Olean),
            ArtifactTrust::build_tree_unknown(path, ArtifactKind::Ilean),
        ]
    } else {
        vec![
            ArtifactTrust::olean_project_missing_build(".lake/build/lib/lean is absent"),
            ArtifactTrust::ilean_project_missing_build(),
        ]
    }
}

fn semantic_issues(
    runtime_error: Option<RuntimeFailure>,
    warnings: Vec<String>,
    next_actions: Vec<String>,
) -> Vec<SemanticIssue> {
    let mut out = Vec::new();
    if let Some(error) = runtime_error {
        let retryable = error.retryable;
        out.push(SemanticIssue {
            code: "runtime_unavailable".to_owned(),
            message: error.reason.clone(),
            severity: Some("error".to_owned()),
            next_action: None,
            retryable: Some(retryable),
            details: serde_json::to_value(error).ok(),
        });
    }
    let mut next_actions = next_actions.into_iter();
    for warning in warnings {
        out.push(SemanticIssue {
            code: "warning".to_owned(),
            message: warning,
            severity: Some("warning".to_owned()),
            next_action: next_actions.next(),
            retryable: None,
            details: None,
        });
    }
    out
}

fn decode<T>(req: SemanticToolRequest) -> std::result::Result<T, Box<SemanticResponse<Value>>>
where
    T: DeserializeOwned,
{
    let value = Value::Object(req.args.into_iter().collect());
    serde_json::from_value(value).map_err(|err| {
        Box::new(SemanticResponse {
            data: None,
            errors: vec![SemanticIssue {
                code: "invalid_request".to_owned(),
                message: err.to_string(),
                severity: Some("error".to_owned()),
                next_action: None,
                retryable: Some(false),
                details: None,
            }],
            trust: SemanticTrust::unknown(),
        })
    })
}

fn missing_kind(tool: &str, allowed: &[&str]) -> SemanticResponse<Value> {
    SemanticResponse {
        data: None,
        errors: vec![SemanticIssue {
            code: "missing_kind".to_owned(),
            message: format!("{tool} requires `kind`; allowed: {}", allowed.join(", ")),
            severity: Some("error".to_owned()),
            next_action: None,
            retryable: Some(false),
            details: None,
        }],
        trust: SemanticTrust::unknown(),
    }
}

fn invalid_kind(tool: &str, kind: &str, allowed: &[&str]) -> SemanticResponse<Value> {
    let message = if allowed.is_empty() {
        format!("{tool} does not support kind `{kind}`; omit `kind` for this tool")
    } else {
        format!("{tool} does not support kind `{kind}`; allowed: {}", allowed.join(", "))
    };
    SemanticResponse {
        data: None,
        errors: vec![SemanticIssue {
            code: "invalid_kind".to_owned(),
            message,
            severity: Some("error".to_owned()),
            next_action: None,
            retryable: Some(false),
            details: None,
        }],
        trust: SemanticTrust::unknown(),
    }
}

fn response_carrier_name(carrier: ResponseCarrier) -> &'static str {
    match carrier {
        ResponseCarrier::Text => "text",
        ResponseCarrier::Structured => "structured",
        ResponseCarrier::Both => "both",
    }
}

fn telemetry_verbosity_name(verbosity: TelemetryVerbosity) -> &'static str {
    match verbosity {
        TelemetryVerbosity::Quiet => "quiet",
        TelemetryVerbosity::Full => "full",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::broker::{BrokerConfig, ProjectBroker};
    use crate::envelope::Freshness;
    use crate::tools::ToolConfig;
    use crate::trust::{ArtifactKind, TrustScope};

    fn make_lake_dir(root: &std::path::Path) -> std::path::PathBuf {
        let dir = root.join("status");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lakefile.lean"), "package status\nlean_lib Status\n").unwrap();
        std::fs::write(dir.join("lean-toolchain"), "leanprover/lean4:v4.31.0-rc2\n").unwrap();
        std::fs::write(dir.join("lake-manifest.json"), "{}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn freshness(root: &std::path::Path) -> Freshness {
        Freshness {
            project_root: root.to_string_lossy().into_owned(),
            project_hash: "hash".to_owned(),
            imports: vec!["Init".to_owned()],
            session_id: "test-session".to_owned(),
            lean_toolchain: "leanprover/lean4:v4.31.0-rc2".to_owned(),
            toolchain_advisories: Vec::new(),
        }
    }

    #[tokio::test]
    async fn lean_status_does_not_open_worker_and_reports_trust_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
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
        let ctx = ToolContext {
            broker: std::sync::Arc::clone(&broker),
            config: ToolConfig::default(),
        };

        let response = lean_status(
            &ctx,
            SemanticToolRequest {
                kind: Some("project".to_owned()),
                args: BTreeMap::from([(
                    "include".to_owned(),
                    serde_json::json!(["toolchain", "worker", "artifacts"]),
                )]),
            },
        )
        .await
        .unwrap();

        assert!(response.errors.is_empty());
        assert_eq!(response.trust.session_id, "metadata-only");
        assert!(response.trust.artifacts.iter().any(|artifact| {
            artifact.artifact == ArtifactKind::Worker
                && artifact.scope == TrustScope::Toolchain
                && artifact.status == TrustStatus::NotApplicable
        }));
        assert!(response.trust.artifacts.iter().any(|artifact| {
            artifact.artifact == ArtifactKind::Olean && artifact.status == TrustStatus::MissingBuild
        }));
        assert!(broker.resident_paths().is_empty());
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn quiet_telemetry_does_not_drop_trust_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let response = Response::ok(serde_json::json!({"status": "ok"}), freshness(tmp.path()))
            .with_trust_artifact(ArtifactTrust::ilean_project_missing_build());

        let semantic = from_tool_response(response, TelemetryVerbosity::Quiet).unwrap();
        let json = serde_json::to_value(&semantic).unwrap();

        assert!(json.get("telemetry").is_none());
        assert_eq!(
            json.pointer("/trust/artifacts/0/artifact")
                .and_then(serde_json::Value::as_str),
            Some("ilean")
        );
        assert_eq!(
            json.pointer("/trust/artifacts/0/status")
                .and_then(serde_json::Value::as_str),
            Some("missing_build")
        );
    }

    #[tokio::test]
    async fn semantic_surface_invalid_kind_is_payload_error() {
        let tmp = tempfile::tempdir().unwrap();
        let root = make_lake_dir(tmp.path());
        let broker = ProjectBroker::new(BrokerConfig {
            config_default: None,
            env_default: Some(root),
            cwd: tmp.path().to_path_buf(),
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: std::time::Duration::ZERO,
            semantic_permits: BrokerConfig::default_semantic_permits(),
            semantic_waiters: BrokerConfig::default_semantic_waiters(),
            semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
            semantic_lock_dir: BrokerConfig::default_semantic_lock_dir(),
        });
        let ctx = ToolContext {
            broker: std::sync::Arc::clone(&broker),
            config: ToolConfig::default(),
        };

        let response = lean_context(
            &ctx,
            SemanticToolRequest {
                kind: Some("raw_hover".to_owned()),
                args: BTreeMap::new(),
            },
        )
        .await
        .unwrap();

        assert!(response.data.is_none());
        let error = response.errors.first();
        assert!(matches!(error.map(|issue| issue.code.as_str()), Some("invalid_kind")));
        assert!(matches!(error.and_then(|issue| issue.retryable), Some(false)));
        drop(ctx);
        drop(broker);
    }

    #[test]
    fn tool_catalog_documents_semantic_surface() {
        let catalog = include_str!("../../../../docs/tool-catalog.md");
        for tool in [
            "lean_context",
            "lean_trial",
            "lean_verify",
            "lean_lookup",
            "lean_status",
        ] {
            assert!(catalog.contains(tool), "catalog should document {tool}");
        }
        for old_heading in [
            "## `proof_state`",
            "## `search_for_proof`",
            "## `inspect_declaration`",
            "## `try_proof_step`",
            "## `verify_declaration`",
            "## `find_references`",
        ] {
            assert!(
                !catalog.contains(old_heading),
                "catalog must not keep old public section {old_heading}"
            );
        }
    }
}
