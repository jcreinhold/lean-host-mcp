//! `project_scan` — filesystem regex sweep over the project's `.lean` files.
//! No Lean session involvement; cheapest tool in the catalogue.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::tools::{ToolContext, new_session_id};

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

pub async fn project_scan(ctx: &ToolContext, req: ProjectScanRequest) -> Result<Response<ProjectScanResult>> {
    let freshness = ctx.freshness(&[], &new_session_id());
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
    let root = Path::new(&ctx.lake_root);
    let mut hits = Vec::new();
    let mut truncated = false;
    'walk: for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_dir(e.file_name().to_str().unwrap_or("")))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("lean") {
            continue;
        }
        let contents = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (idx, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                if hits.len() >= limit {
                    truncated = true;
                    break 'walk;
                }
                hits.push(ProjectScanHit {
                    file: entry
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .into_owned(),
                    line: (idx + 1) as u32,
                    text: line.trim_end().to_owned(),
                });
            }
        }
    }
    Ok(Response::ok(ProjectScanResult { hits, truncated }, freshness))
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".lake" | ".git" | "target" | "build" | "node_modules" | ".direnv")
}
