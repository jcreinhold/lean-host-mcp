//! Typed trust facts surfaced by the semantic MCP facade.
//!
//! These rows are intentionally small and proof-relevant: they describe the
//! source/build/worker artifacts a verdict depends on. Operational counters,
//! cache timings, and import lists stay in telemetry.

use std::collections::HashSet;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustStatus {
    EditFresh,
    BuildFresh,
    StaleBuild,
    MissingBuild,
    /// An import names no module: Lake finds none by that name, so no build
    /// can supply its `.olean`.
    UnknownImport,
    Unknown,
    NotApplicable,
}

/// The next action that builds `modules` with `lean_build`, then retries, or
/// that builds the project's default targets when `modules` is empty. Each
/// module is a `+NAME` target, so Lake reads it as a module and not as a
/// library or package of the same name.
pub(crate) fn build_action<S: AsRef<str>>(modules: &[S]) -> String {
    if modules.is_empty() {
        return "run lean_build() to build the default targets, then retry".to_owned();
    }
    let targets: Vec<String> = modules.iter().map(|module| format!("+{}", module.as_ref())).collect();
    let targets = serde_json::to_string(&targets).unwrap_or_default();
    format!("run lean_build(targets={targets}), then retry")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Source,
    Olean,
    Ilean,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustScope {
    File,
    Module,
    Project,
    Toolchain,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactTrust {
    pub artifact: ArtifactKind,
    pub scope: TrustScope,
    pub status: TrustStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    /// SHA-256 of the exact source bytes used for this semantic call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
}

impl ArtifactTrust {
    pub fn new(artifact: ArtifactKind, scope: TrustScope, status: TrustStatus) -> Self {
        Self {
            artifact,
            scope,
            status,
            path: None,
            module: None,
            detail: None,
            next_action: None,
            content_sha256: None,
        }
    }

    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn module(mut self, module: impl Into<String>) -> Self {
        self.module = Some(module.into());
        self
    }

    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_action = Some(next_action.into());
        self
    }

    #[must_use]
    pub fn content_sha256(mut self, digest: [u8; 32]) -> Self {
        fn hex_digit(nibble: u8) -> char {
            match nibble {
                0 => '0',
                1 => '1',
                2 => '2',
                3 => '3',
                4 => '4',
                5 => '5',
                6 => '6',
                7 => '7',
                8 => '8',
                9 => '9',
                10 => 'a',
                11 => 'b',
                12 => 'c',
                13 => 'd',
                14 => 'e',
                15 => 'f',
                _ => '?',
            }
        }

        let encoded = digest.iter().fold(String::with_capacity(64), |mut output, byte| {
            output.push(hex_digit(*byte >> 4));
            output.push(hex_digit(*byte & 0x0f));
            output
        });
        self.content_sha256 = Some(encoded);
        self
    }

    pub fn source_file_edit_fresh(root: &Path, path: &Path, digest: [u8; 32]) -> Self {
        Self::new(ArtifactKind::Source, TrustScope::File, TrustStatus::EditFresh)
            .path(display_path(root, path))
            .content_sha256(digest)
            .detail("source snapshot was read from disk for this call")
    }

    pub fn ilean_project_build_fresh() -> Self {
        Self::new(ArtifactKind::Ilean, TrustScope::Project, TrustStatus::BuildFresh)
            .detail("project reference index is current for contributing modules")
    }

    pub fn ilean_project_stale_build(detail: impl Into<String>) -> Self {
        Self::new(ArtifactKind::Ilean, TrustScope::Project, TrustStatus::StaleBuild)
            .detail(detail)
            .next_action(build_action::<&str>(&[]))
    }

    pub fn ilean_project_missing_build() -> Self {
        Self::new(ArtifactKind::Ilean, TrustScope::Project, TrustStatus::MissingBuild)
            .detail(".lake/build/lib/lean is absent")
            .next_action(build_action::<&str>(&[]))
    }

    pub fn ilean_module_build_fresh(module: impl Into<String>, path: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Ilean, TrustScope::Module, TrustStatus::BuildFresh)
            .module(module.clone())
            .path(path)
            .detail(format!(
                "module `{module}` declaration index is available from the last build"
            ))
    }

    pub fn ilean_module_stale_build(module: impl Into<String>, path: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Ilean, TrustScope::Module, TrustStatus::StaleBuild)
            .module(module.clone())
            .path(path)
            .detail(format!(
                "module `{module}` source is newer than its .ilean declaration index"
            ))
            .next_action(build_action(&[&module]))
    }

    pub fn ilean_module_missing_build(module: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Ilean, TrustScope::Module, TrustStatus::MissingBuild)
            .module(module.clone())
            .detail(format!("module `{module}` has no built .ilean declaration index"))
            .next_action(build_action(&[&module]))
    }

    pub fn olean_project_missing_build(detail: impl Into<String>) -> Self {
        Self::new(ArtifactKind::Olean, TrustScope::Project, TrustStatus::MissingBuild)
            .detail(detail)
            .next_action(build_action::<&str>(&[]))
    }

    pub fn olean_project_stale_build(detail: impl Into<String>) -> Self {
        Self::new(ArtifactKind::Olean, TrustScope::Project, TrustStatus::StaleBuild)
            .detail(detail)
            .next_action(build_action::<&str>(&[]))
    }

    pub fn olean_module_stale_build(module: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Olean, TrustScope::Module, TrustStatus::StaleBuild)
            .module(module.clone())
            .detail(format!("Lake reports module `{module}` out of date"))
            .next_action(build_action(&[&module]))
    }

    pub fn olean_module_missing_build(module: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Olean, TrustScope::Module, TrustStatus::MissingBuild)
            .module(module.clone())
            .detail(format!("module `{module}` is missing a built .olean"))
            .next_action(build_action(&[&module]))
    }

    /// The row for an import that names no module ([`TrustStatus::UnknownImport`]).
    pub fn olean_module_unknown_import(module: impl Into<String>) -> Self {
        let module = module.into();
        Self::new(ArtifactKind::Olean, TrustScope::Module, TrustStatus::UnknownImport)
            .module(module.clone())
            .detail(format!(
                "Lake finds no module `{module}`, so no build can supply its .olean"
            ))
            .next_action(format!("fix the import of `{module}`, then retry"))
    }

    pub fn worker_toolchain_unknown(detail: impl Into<String>) -> Self {
        Self::new(ArtifactKind::Worker, TrustScope::Toolchain, TrustStatus::Unknown).detail(detail)
    }

    pub fn worker_toolchain_not_applicable(detail: impl Into<String>) -> Self {
        Self::new(ArtifactKind::Worker, TrustScope::Toolchain, TrustStatus::NotApplicable).detail(detail)
    }

    pub fn build_tree_unknown(path: impl Into<String>, artifact: ArtifactKind) -> Self {
        Self::new(artifact, TrustScope::Project, TrustStatus::Unknown)
            .path(path)
            .detail("build tree exists; lean_status does not compare source mtimes")
            .next_action("run lean_lookup(kind=\"references\" or kind=\"declarations\") for semantic freshness, or lean_build() to refresh artifacts")
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ArtifactTrustDeduper {
    seen: HashSet<ArtifactTrust>,
    artifacts: Vec<ArtifactTrust>,
}

impl ArtifactTrustDeduper {
    pub(crate) fn push(&mut self, artifact: ArtifactTrust) {
        if self.seen.insert(artifact.clone()) {
            self.artifacts.push(artifact);
        }
    }

    pub(crate) fn extend(&mut self, artifacts: impl IntoIterator<Item = ArtifactTrust>) {
        for artifact in artifacts {
            self.push(artifact);
        }
    }

    pub(crate) fn into_vec(self) -> Vec<ArtifactTrust> {
        self.artifacts
    }
}

pub(crate) fn dedupe_artifacts(artifacts: impl IntoIterator<Item = ArtifactTrust>) -> Vec<ArtifactTrust> {
    let mut deduper = ArtifactTrustDeduper::default();
    deduper.extend(artifacts);
    deduper.into_vec()
}

/// The digest of the one source text a reply's result came from.
///
/// A check that read one file reports that file's `content_sha256`, so the
/// caller can compare it with the text it meant to check. `None` when the
/// reply read no file, or read two files or two versions of one file; each
/// `source` row still carries its own digest then. Pass deduplicated rows, or
/// two reads of the same unchanged file count as two texts.
pub(crate) fn checked_source_sha256(artifacts: &[ArtifactTrust]) -> Option<String> {
    let mut sources = artifacts
        .iter()
        .filter(|artifact| artifact.artifact == ArtifactKind::Source && artifact.scope == TrustScope::File);
    let only = sources.next()?;
    if sources.next().is_some() {
        return None;
    }
    only.content_sha256.clone()
}

pub(crate) fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn trust_status_tokens_are_snake_case_and_round_trip() {
        let statuses = [
            (TrustStatus::EditFresh, "edit_fresh"),
            (TrustStatus::BuildFresh, "build_fresh"),
            (TrustStatus::StaleBuild, "stale_build"),
            (TrustStatus::MissingBuild, "missing_build"),
            (TrustStatus::UnknownImport, "unknown_import"),
            (TrustStatus::Unknown, "unknown"),
            (TrustStatus::NotApplicable, "not_applicable"),
        ];
        for (status, token) in statuses {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{token}\""));
            assert_eq!(serde_json::from_str::<TrustStatus>(&json).unwrap(), status);
        }
    }

    #[test]
    fn build_rows_name_lean_build_with_module_targets() {
        assert_eq!(
            ArtifactTrust::olean_module_missing_build("Chain.D")
                .next_action
                .as_deref(),
            Some(r#"run lean_build(targets=["+Chain.D"]), then retry"#)
        );
        assert_eq!(
            build_action(&["A", "B.C"]),
            r#"run lean_build(targets=["+A","+B.C"]), then retry"#
        );
        assert_eq!(
            build_action::<&str>(&[]),
            "run lean_build() to build the default targets, then retry"
        );
        let typo = ArtifactTrust::olean_module_unknown_import("Chain.Typo");
        assert_eq!(
            (typo.status, typo.module.as_deref()),
            (TrustStatus::UnknownImport, Some("Chain.Typo"))
        );
        assert!(!typo.next_action.unwrap_or_default().contains("lean_build"));
    }

    #[test]
    fn representative_trust_fact_round_trips() {
        let fact = ArtifactTrust::new(ArtifactKind::Source, TrustScope::File, TrustStatus::EditFresh)
            .path("A.lean")
            .detail("read for this call");
        let json = serde_json::to_value(&fact).unwrap();
        assert_eq!(
            json.pointer("/artifact").and_then(serde_json::Value::as_str),
            Some("source")
        );
        assert_eq!(json.pointer("/scope").and_then(serde_json::Value::as_str), Some("file"));
        assert_eq!(
            json.pointer("/status").and_then(serde_json::Value::as_str),
            Some("edit_fresh")
        );
        assert_eq!(serde_json::from_value::<ArtifactTrust>(json).unwrap(), fact);
    }

    #[test]
    fn trust_deduplication_preserves_first_occurrence_order() {
        let source =
            ArtifactTrust::source_file_edit_fresh(Path::new("/tmp/project"), Path::new("/tmp/project/A.lean"), [7; 32]);
        let ilean = ArtifactTrust::ilean_project_build_fresh();
        let deduped = dedupe_artifacts([source.clone(), ilean.clone(), source.clone()]);
        assert_eq!(deduped, vec![source, ilean]);
    }

    #[test]
    fn a_reply_names_a_source_digest_only_for_one_text() {
        let root = Path::new("/tmp/project");
        let first = ArtifactTrust::source_file_edit_fresh(root, Path::new("/tmp/project/A.lean"), [0xab; 32]);
        let ilean = ArtifactTrust::ilean_project_build_fresh();
        assert_eq!(
            checked_source_sha256(&[ilean.clone(), first.clone()]),
            Some("ab".repeat(32))
        );
        assert_eq!(checked_source_sha256(std::slice::from_ref(&ilean)), None);
        let other_file = ArtifactTrust::source_file_edit_fresh(root, Path::new("/tmp/project/B.lean"), [0xab; 32]);
        assert_eq!(checked_source_sha256(&[first.clone(), other_file]), None);
        let edited = ArtifactTrust::source_file_edit_fresh(root, Path::new("/tmp/project/A.lean"), [0xcd; 32]);
        assert_eq!(checked_source_sha256(&[first, edited]), None);
    }

    #[test]
    fn source_fact_serializes_the_snapshot_digest() {
        let fact = ArtifactTrust::source_file_edit_fresh(
            Path::new("/tmp/project"),
            Path::new("/tmp/project/A.lean"),
            [0xab; 32],
        );
        let expected = "ab".repeat(32);
        assert_eq!(fact.content_sha256.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn display_path_prefers_project_relative_paths_inside_root() {
        assert_eq!(
            display_path(Path::new("/tmp/project"), Path::new("/tmp/project/A/B.lean")),
            "A/B.lean"
        );
        assert_eq!(
            display_path(Path::new("/tmp/project"), Path::new("/other/A.lean")),
            "/other/A.lean"
        );
    }
}
