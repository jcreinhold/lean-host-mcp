//! `project_scan` — filesystem regex sweep over the project's `.lean` files.
//! No Lean session involvement; cheapest tool in the catalogue.
//!
//! Routes through [`ProjectBroker::resolve_meta`] rather than
//! [`ProjectBroker::with_project`] so a broken worker bootstrap (missing
//! toolchain, bad lakefile target, etc.) doesn't block a pure filesystem
//! operation. The trade-off is documented on
//! [`freshness_for_meta`](crate::tools::freshness_for_meta): `session_id`
//! reflects this call rather than a long-lived actor.

// Same ownership rationale as `tools::lean`.
#![allow(clippy::needless_pass_by_value)]
// `project_scan` is worker-free, so the body has no `.await`. Keep `async`
// for dispatcher symmetry with the other tool handlers in [`server.rs`] —
// the cost of one suppressed lint is much smaller than the cost of a
// special case in the rmcp glue.
#![allow(clippy::unused_async)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::tools::{ToolContext, freshness_for_meta, is_ignored_dir};

const MAX_HITS: usize = 1000;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProjectScanRequest {
    /// Named preset or raw regex. Presets: `sorry`, `admit`, `axiom`,
    /// `set_option`. Raw regex requires `preset = "custom"` and `pattern`
    /// to be set.
    pub preset: Preset,
    /// Required when `preset = "custom"`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Cap the number of returned hits.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional explicit project (absolute path to Lake root). When
    /// omitted, the server resolves via env → cwd-walk → config default.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Sorry,
    Admit,
    Axiom,
    SetOption,
    Custom,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectScanHit {
    pub file: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProjectScanResult {
    pub hits: Vec<ProjectScanHit>,
    pub truncated: bool,
}

/// # Errors
///
/// Returns `ServerError::Internal` if the custom preset omits a pattern or
/// the supplied pattern fails to compile as a regex.
pub async fn project_scan(ctx: &ToolContext, req: ProjectScanRequest) -> Result<Response<ProjectScanResult>> {
    let hint = ProjectHint::from_request(req.project);
    let meta = ctx.broker.resolve_meta(&hint)?;
    let freshness = freshness_for_meta(&meta);
    let pattern = match req.preset {
        Preset::Sorry => r"\bsorry\b".to_owned(),
        Preset::Admit => r"\badmit\b".to_owned(),
        Preset::Axiom => r"^\s*axiom\s".to_owned(),
        Preset::SetOption => r"^\s*set_option\s".to_owned(),
        Preset::Custom => req
            .pattern
            .clone()
            .ok_or_else(|| ServerError::Internal("custom preset requires pattern".into()))?,
    };
    let re = regex::Regex::new(&pattern).map_err(|e| ServerError::Internal(format!("regex {pattern}: {e}")))?;
    let limit = req.limit.unwrap_or(MAX_HITS).min(MAX_HITS);
    let root = meta.canonical_root.as_path();
    let mut hits = Vec::new();
    let mut truncated = false;
    'walk: for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("lean") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (idx, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                if hits.len() >= limit {
                    truncated = true;
                    break 'walk;
                }
                let line_no = u32::try_from(idx.saturating_add(1)).unwrap_or(u32::MAX);
                let rel_path = entry.path().strip_prefix(root).unwrap_or_else(|_| entry.path());
                hits.push(ProjectScanHit {
                    file: rel_path.to_string_lossy().into_owned(),
                    line: line_no,
                    text: line.trim_end().to_owned(),
                });
            }
        }
    }
    Ok(Response::ok(ProjectScanResult { hits, truncated }, freshness))
}
