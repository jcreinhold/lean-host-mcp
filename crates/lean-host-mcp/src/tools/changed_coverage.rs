//! Git-diff to declaration coverage mapping for `lean_verify` and `lean_lookup`.
//!
//! This module owns only selection: git parsing, source-fresh declaration
//! inventory calls, and conservative coverage gaps. Verification stays in
//! `proof_action`.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::broker::ProjectHint;
use crate::envelope::Response;
use crate::error::{Result, ServerError};
use crate::tools::ToolContext;
use crate::tools::declaration_inventory::{
    DeclarationInventoryRequest, DeclarationInventoryRow, DeclarationInventoryTarget, DeclarationSpan,
    declaration_inventory,
};
use crate::tools::source_input::resolve_path;
use crate::trust::ArtifactTrustDeduper;

const DEFAULT_BASE: &str = "HEAD";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ChangedCoverageRequest {
    /// Git base ref to compare against. Defaults to `HEAD`.
    #[serde(default)]
    pub base: Option<String>,
    /// Optional changed-file restriction. Paths are relative to the project
    /// root unless absolute.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Include untracked Lean files as whole-file changes.
    #[serde(default)]
    pub include_untracked: bool,
    /// Project-root override; defaults to the server's configured Lake project.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub(crate) struct ChangedCoverageResult {
    pub known: Vec<ChangedDeclaration>,
    pub coverage: ChangedCoverageReport,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ChangedCoverageReport {
    pub unknown: Vec<UnknownCoverage>,
    pub deleted_files: Vec<DeletedFile>,
    pub renamed_files: Vec<RenamedFile>,
    pub truncated: bool,
}

impl ChangedCoverageReport {
    pub(crate) fn is_empty(&self) -> bool {
        self.unknown.is_empty() && self.deleted_files.is_empty() && self.renamed_files.is_empty() && !self.truncated
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.unknown.extend(other.unknown);
        self.deleted_files.extend(other.deleted_files);
        self.renamed_files.extend(other.renamed_files);
        self.truncated |= other.truncated;
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub(crate) struct ChangedDeclaration {
    pub file: String,
    pub declaration: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UnknownCoverage {
    pub file: String,
    pub reason: String,
    pub next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeletedFile {
    pub file: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenamedFile {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default)]
struct ChangedFile {
    path: String,
    hunks: Vec<LineInterval>,
    whole_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineInterval {
    start: u32,
    end: u32,
}

/// Report declaration coverage for git changes without verifying anything.
///
/// # Errors
///
/// Returns infrastructure failures: project resolution, git invocation, or
/// declaration-inventory runtime failures.
pub(crate) async fn changed_coverage(
    ctx: &ToolContext,
    req: ChangedCoverageRequest,
) -> Result<Response<ChangedCoverageResult>> {
    let hint = ProjectHint::from_request(req.project.clone());
    let meta = ctx.broker.resolve_meta(&hint)?;
    let coverage = compute_changed_coverage(ctx, hint.clone(), &meta.canonical_root, req).await?;
    if coverage.result_ref().is_some() {
        return Ok(coverage);
    }
    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    Ok(Response::ok(ChangedCoverageResult::default(), base.freshness).with_runtime(base.runtime))
}

pub(crate) async fn compute_changed_coverage(
    ctx: &ToolContext,
    hint: ProjectHint,
    root: &Path,
    req: ChangedCoverageRequest,
) -> Result<Response<ChangedCoverageResult>> {
    let base_ref = req.base.as_deref().unwrap_or(DEFAULT_BASE);
    let file_filter = normalize_file_filter(root, &req.files)?;
    let diff = git_diff(root, base_ref, &file_filter)?;
    let mut parsed = parse_git_diff(&diff);
    if req.include_untracked {
        for file in git_untracked(root, &file_filter)? {
            parsed
                .changed
                .entry(file.clone())
                .or_insert_with(|| ChangedFile {
                    path: file,
                    hunks: Vec::new(),
                    whole_file: true,
                })
                .whole_file = true;
        }
    }

    let mut known = Vec::new();
    let mut report = ChangedCoverageReport {
        unknown: Vec::new(),
        deleted_files: parsed.deleted.into_iter().map(|file| DeletedFile { file }).collect(),
        renamed_files: parsed
            .renamed
            .into_iter()
            .map(|(from, to)| RenamedFile { from, to })
            .collect(),
        truncated: false,
    };
    let mut trust_artifacts = ArtifactTrustDeduper::default();
    let mut warnings = Vec::new();
    let mut next_actions = Vec::new();

    for changed in parsed.changed.into_values() {
        let inventory = declaration_inventory(
            ctx,
            DeclarationInventoryRequest {
                target: DeclarationInventoryTarget::File {
                    path: PathBuf::from(&changed.path),
                },
                project: req.project.clone(),
                limit: None,
            },
        )
        .await?;
        trust_artifacts.extend(inventory.trust_artifacts.iter().cloned());
        warnings.extend(inventory.warnings.clone());
        next_actions.extend(inventory.next_actions.clone());
        let Some(result) = inventory.result_ref() else {
            report
                .unknown
                .push(unknown_file(changed.path, "declaration_inventory_unavailable", None));
            continue;
        };
        if result.status != "ok" {
            report
                .unknown
                .push(unknown_file(changed.path, "declaration_inventory_unavailable", None));
            continue;
        }
        if result.truncated {
            report.truncated = true;
            report.unknown.push(unknown_file(
                changed.path.clone(),
                "declaration_inventory_truncated",
                None,
            ));
        }
        let mapped = map_changed_file(&changed, &result.declarations);
        known.extend(mapped.known);
        report.unknown.extend(mapped.unknown);
    }

    let base = ctx.broker.project_identity_without_worker(&hint, Vec::new())?;
    let mut response = Response::ok(
        ChangedCoverageResult {
            known,
            coverage: report,
        },
        base.freshness,
    )
    .with_runtime(base.runtime)
    .with_trust_artifacts(trust_artifacts.into_vec());
    response.warnings.extend(warnings);
    response.next_actions.extend(next_actions);
    Ok(response)
}

#[derive(Debug, Default)]
struct ParsedDiff {
    changed: BTreeMap<String, ChangedFile>,
    deleted: Vec<String>,
    renamed: Vec<(String, String)>,
}

fn git_diff(root: &Path, base: &str, file_filter: &BTreeSet<String>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["diff", "--unified=0", "--no-ext-diff", "--find-renames", base, "--"]);
    if file_filter.is_empty() {
        cmd.arg("*.lean");
    } else {
        cmd.args(file_filter);
    }
    command_output(cmd, "git diff")
}

fn git_untracked(root: &Path, file_filter: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["ls-files", "--others", "--exclude-standard", "--"]);
    if file_filter.is_empty() {
        cmd.arg("*.lean");
    } else {
        cmd.args(file_filter);
    }
    let out = command_output(cmd, "git ls-files")?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| Path::new(line).extension().is_some_and(|ext| ext == "lean"))
        .map(ToOwned::to_owned)
        .collect())
}

fn command_output(mut cmd: Command, label: &str) -> Result<String> {
    let output = cmd.output().map_err(ServerError::Io)?;
    let text = String::from_utf8_lossy(&output.stdout).to_string() + String::from_utf8_lossy(&output.stderr).as_ref();
    if output.status.success() {
        Ok(text)
    } else {
        Err(ServerError::BadProject(format!(
            "{label} failed: {}",
            text.lines()
                .rev()
                .take(12)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        )))
    }
}

fn normalize_file_filter(root: &Path, files: &[PathBuf]) -> Result<BTreeSet<String>> {
    files
        .iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "lean"))
        .map(|path| {
            let resolved = resolve_path(root, path);
            let relative = resolved.strip_prefix(root).map_err(|_| {
                ServerError::BadProject(format!(
                    "changed coverage file `{}` is outside project root `{}`",
                    resolved.display(),
                    root.display()
                ))
            })?;
            Ok(relative.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

fn parse_git_diff(diff: &str) -> ParsedDiff {
    let mut parsed = ParsedDiff::default();
    let mut current: Option<String> = None;
    let mut current_deleted = false;
    let mut rename_from: Option<String> = None;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(from) = rename_from.take()
                && let Some(to) = current.clone()
            {
                parsed.renamed.push((from, to));
            }
            current = parse_diff_git_new_path(rest);
            current_deleted = false;
            continue;
        }
        if line == "deleted file mode 100644" || line.starts_with("deleted file mode ") {
            current_deleted = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            rename_from = Some(rest.to_owned());
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename to ") {
            let to = rest.to_owned();
            if let Some(from) = rename_from.take() {
                parsed.renamed.push((from, to.clone()));
            }
            current = Some(to);
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                if let Some(file) = current.take() {
                    parsed.deleted.push(file);
                }
                current_deleted = true;
            } else if let Some(path) = rest.strip_prefix("b/") {
                current = Some(path.to_owned());
            }
            continue;
        }
        if let Some(interval) = line.strip_prefix("@@").and_then(parse_hunk_interval)
            && let Some(file) = current.clone()
            && !current_deleted
        {
            parsed
                .changed
                .entry(file.clone())
                .or_insert_with(|| ChangedFile {
                    path: file,
                    hunks: Vec::new(),
                    whole_file: false,
                })
                .hunks
                .push(interval);
        }
    }
    if let Some(from) = rename_from.take()
        && let Some(to) = current
    {
        parsed.renamed.push((from, to));
    }
    parsed.deleted.sort();
    parsed.deleted.dedup();
    parsed.renamed.sort();
    parsed.renamed.dedup();
    parsed
}

fn parse_diff_git_new_path(rest: &str) -> Option<String> {
    let mut parts = rest.split_whitespace();
    let _old = parts.next()?;
    parts.next()?.strip_prefix("b/").map(ToOwned::to_owned)
}

fn parse_hunk_interval(hunk_tail: &str) -> Option<LineInterval> {
    let plus = hunk_tail.split_whitespace().find(|part| part.starts_with('+'))?;
    let nums = plus.trim_start_matches('+');
    let (start, len) = match nums.split_once(',') {
        Some((start, len)) => (start.parse::<u32>().ok()?, len.parse::<u32>().ok()?),
        None => (nums.parse::<u32>().ok()?, 1),
    };
    Some(LineInterval {
        start: start.max(1),
        end: if len == 0 {
            start.max(1)
        } else {
            start.saturating_add(len).saturating_sub(1).max(1)
        },
    })
}

#[derive(Debug, Default)]
struct MappedCoverage {
    known: Vec<ChangedDeclaration>,
    unknown: Vec<UnknownCoverage>,
}

fn map_changed_file(changed: &ChangedFile, declarations: &[DeclarationInventoryRow]) -> MappedCoverage {
    if changed.whole_file {
        return MappedCoverage {
            known: declarations
                .iter()
                .map(|row| ChangedDeclaration {
                    file: changed.path.clone(),
                    declaration: row.name.clone(),
                    reason: "whole_file_changed".to_owned(),
                })
                .collect(),
            unknown: if declarations.is_empty() {
                vec![unknown_file(
                    changed.path.clone(),
                    "no_declarations_in_changed_file",
                    None,
                )]
            } else {
                Vec::new()
            },
        };
    }

    let mut known_by_decl = BTreeMap::<String, ChangedDeclaration>::new();
    let mut unknown = Vec::new();
    for hunk in &changed.hunks {
        let mut matched = false;
        for row in declarations {
            if let Some(reason) = overlap_reason(*hunk, row) {
                matched = true;
                known_by_decl
                    .entry(row.name.clone())
                    .or_insert_with(|| ChangedDeclaration {
                        file: changed.path.clone(),
                        declaration: row.name.clone(),
                        reason,
                    });
            }
        }
        if !matched {
            unknown.push(unknown_file(
                changed.path.clone(),
                "hunk_outside_declaration",
                Some(*hunk),
            ));
        }
    }
    MappedCoverage {
        known: known_by_decl.into_values().collect(),
        unknown,
    }
}

fn overlap_reason(hunk: LineInterval, row: &DeclarationInventoryRow) -> Option<String> {
    if overlaps(hunk, &row.name_span) {
        return Some("hunk_overlaps_name".to_owned());
    }
    if row.body_span.as_ref().is_some_and(|span| overlaps(hunk, span)) {
        return Some("hunk_overlaps_body".to_owned());
    }
    overlaps(hunk, &row.declaration_span).then(|| "hunk_overlaps_declaration".to_owned())
}

fn overlaps(hunk: LineInterval, span: &DeclarationSpan) -> bool {
    hunk.start <= span.end_line && span.start_line <= hunk.end
}

fn unknown_file(file: String, reason: &str, hunk: Option<LineInterval>) -> UnknownCoverage {
    UnknownCoverage {
        file,
        reason: reason.to_owned(),
        next_action: "verify the whole file or run lake build and retry".to_owned(),
        line_start: hunk.map(|h| h.start),
        line_end: hunk.map(|h| h.end),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn span(start: u32, end: u32) -> DeclarationSpan {
        DeclarationSpan {
            start_line: start,
            start_column: 1,
            end_line: end,
            end_column: 1,
        }
    }

    fn row(name: &str) -> DeclarationInventoryRow {
        DeclarationInventoryRow {
            name: name.to_owned(),
            short_name: name.rsplit('.').next().unwrap().to_owned(),
            kind: Some("theorem".to_owned()),
            declaration_span: span(3, 8),
            name_span: span(3, 3),
            body_span: Some(span(4, 8)),
        }
    }

    #[test]
    fn changed_coverage_parse_diff_tracks_hunks_deleted_and_renamed() {
        let diff = "\
diff --git a/Foo.lean b/Foo.lean
--- a/Foo.lean
+++ b/Foo.lean
@@ -4,0 +5,2 @@
diff --git a/Old.lean b/New.lean
similarity index 80%
rename from Old.lean
rename to New.lean
--- a/Old.lean
+++ b/New.lean
@@ -1 +1 @@
diff --git a/Dead.lean b/Dead.lean
deleted file mode 100644
--- a/Dead.lean
+++ /dev/null
@@ -1 +0,0 @@
";
        let parsed = parse_git_diff(diff);
        assert_eq!(parsed.changed["Foo.lean"].hunks[0], LineInterval { start: 5, end: 6 });
        assert_eq!(parsed.renamed, vec![("Old.lean".to_owned(), "New.lean".to_owned())]);
        assert_eq!(parsed.deleted, vec!["Dead.lean"]);
    }

    #[test]
    fn changed_coverage_maps_body_and_name_hunks() {
        let declarations = vec![row("Demo.foo")];
        let body = map_changed_file(
            &ChangedFile {
                path: "Demo.lean".to_owned(),
                hunks: vec![LineInterval { start: 5, end: 5 }],
                whole_file: false,
            },
            &declarations,
        );
        assert_eq!(body.known[0].reason, "hunk_overlaps_body");

        let name = map_changed_file(
            &ChangedFile {
                path: "Demo.lean".to_owned(),
                hunks: vec![LineInterval { start: 3, end: 3 }],
                whole_file: false,
            },
            &declarations,
        );
        assert_eq!(name.known[0].reason, "hunk_overlaps_name");
    }

    #[test]
    fn changed_coverage_unknown_when_hunk_misses_declarations() {
        let mapped = map_changed_file(
            &ChangedFile {
                path: "Demo.lean".to_owned(),
                hunks: vec![LineInterval { start: 1, end: 1 }],
                whole_file: false,
            },
            &[row("Demo.foo")],
        );
        assert!(mapped.known.is_empty());
        assert_eq!(mapped.unknown[0].reason, "hunk_outside_declaration");
    }

    #[test]
    fn changed_coverage_untracked_selects_whole_file() {
        let mapped = map_changed_file(
            &ChangedFile {
                path: "Demo.lean".to_owned(),
                hunks: Vec::new(),
                whole_file: true,
            },
            &[row("Demo.foo"), row("Demo.bar")],
        );
        assert_eq!(mapped.known.len(), 2);
        assert!(mapped.unknown.is_empty());
        assert!(mapped.known.iter().all(|decl| decl.reason == "whole_file_changed"));
    }
}
