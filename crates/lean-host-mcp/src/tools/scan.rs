//! `source_search`: filesystem source sweep over the project's `.lean` files.
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
// `source_search` is worker-free, so the body has no `.await`. Keep `async`
// for dispatcher symmetry with the other tool handlers in [`server.rs`];
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

const MAX_MATCHES: usize = 1000;
const DEFAULT_MAX_FILES_SCANNED: usize = 2000;
const MAX_FILES_SCANNED: usize = 10_000;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SourceSearchRequest {
    pub preset: Preset,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub max_files_scanned: Option<usize>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Sorry,
    Admit,
    Axiom,
    Imports,
    Namespaces,
    DeclarationNames,
    TheoremStatements,
    Custom,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SourceSearchMatch {
    pub file: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SourceSearchResult {
    pub matches: Vec<SourceSearchMatch>,
    pub files_scanned: usize,
    pub files_skipped: usize,
    pub truncated: bool,
    pub source_based: bool,
}

/// # Errors
///
/// Returns `ServerError::Internal` if the custom preset omits a pattern or
/// the supplied pattern fails to compile as a regex.
pub async fn source_search(ctx: &ToolContext, req: SourceSearchRequest) -> Result<Response<SourceSearchResult>> {
    let hint = ProjectHint::from_request(req.project);
    let meta = ctx.broker.resolve_meta(&hint)?;
    let freshness = freshness_for_meta(&meta);
    let pattern = match req.preset {
        Preset::Sorry => r"\bsorry\b".to_owned(),
        Preset::Admit => r"\badmit\b".to_owned(),
        Preset::Axiom => r"^\s*axiom\s".to_owned(),
        Preset::Imports => r"^\s*(?:public\s+)?import\s+\S+".to_owned(),
        Preset::Namespaces => r"^\s*namespace\s+\S+".to_owned(),
        Preset::DeclarationNames => {
            r"^\s*(?:private\s+|protected\s+|noncomputable\s+|unsafe\s+|partial\s+)*(?:def|theorem|lemma|class|structure|inductive|instance|axiom)\s+\S+".to_owned()
        }
        Preset::TheoremStatements => r"^\s*(?:theorem|lemma)\s+\S+".to_owned(),
        Preset::Custom => req
            .pattern
            .clone()
            .ok_or_else(|| ServerError::Internal("custom preset requires pattern".into()))?,
    };
    let re = regex::Regex::new(&pattern).map_err(|e| ServerError::Internal(format!("regex {pattern}: {e}")))?;
    let limit = req.limit.unwrap_or(MAX_MATCHES).min(MAX_MATCHES);
    let max_files_scanned = req
        .max_files_scanned
        .unwrap_or(DEFAULT_MAX_FILES_SCANNED)
        .clamp(1, MAX_FILES_SCANNED);
    let root = meta.canonical_root.as_path();
    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
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
        if files_scanned >= max_files_scanned {
            files_skipped = files_skipped.saturating_add(1);
            truncated = true;
            break 'walk;
        }
        let Ok(contents) = std::fs::read_to_string(entry.path()) else {
            files_skipped = files_skipped.saturating_add(1);
            continue;
        };
        files_scanned = files_scanned.saturating_add(1);
        for (idx, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                if matches.len() >= limit {
                    truncated = true;
                    break 'walk;
                }
                let line_no = u32::try_from(idx.saturating_add(1)).unwrap_or(u32::MAX);
                let rel_path = entry.path().strip_prefix(root).unwrap_or_else(|_| entry.path());
                matches.push(SourceSearchMatch {
                    file: rel_path.to_string_lossy().into_owned(),
                    line: line_no,
                    text: line.trim_end().to_owned(),
                });
            }
        }
    }
    Ok(Response::ok(
        SourceSearchResult {
            matches,
            files_scanned,
            files_skipped,
            truncated,
            source_based: true,
        },
        freshness,
    ))
}
