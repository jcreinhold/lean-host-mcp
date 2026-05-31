//! Resolution-health rendering: the parent's one set of honest, actionable
//! messages for the two ways a name fails to resolve usefully — the project
//! environment is incomplete (`needs_build`), or the name is genuinely
//! ambiguous (multiple competing declarations).
//!
//! Ousterhout ch.10 (exception aggregation). The incomplete-build condition
//! used to reach the parent in several disguises — a worker `MissingImports`
//! outcome, a missing-`.olean` [`ServerError`], a `verification_status` of
//! "ambiguous", and a free-text "ambiguous in the module" message — each
//! presented differently (a hard error here, a misleading "ambiguous" there).
//! This module collapses them into one renderer per verdict so every tool
//! emits the *same* wording. Per ch.10.10 the build-state is deliberately
//! *exposed* (the agent must learn to run `lake build`), never masked.
//!
//! As of `lean-rs-worker-protocol` 0.1.18 the worker classifies resolution at
//! its own boundary: `proof_state` and `verify_declaration` return typed
//! `NeedsBuild` / `Ambiguous { candidates }` verdicts, so the parent no longer
//! string-matches those. The one remaining text heuristic,
//! [`missing_olean_failure`], covers the env-based query path
//! (`find_references`, `search_for_proof`), where an unbuilt transitive
//! dependency still surfaces as an infrastructure [`ServerError`].

use schemars::JsonSchema;
use serde::Serialize;

use crate::envelope::Response;
use crate::error::ServerError;

/// Why a query ran against an environment that was not fully built. Each
/// variant carries only what the renderer needs to name the blocking work.
pub(crate) enum IncompleteCause {
    /// Modules the open environment did not have (worker `MissingImports`).
    MissingImports(Vec<String>),
    /// A query hit a missing `.olean` (an infrastructure [`ServerError`] whose
    /// text [`missing_olean_failure`] recognised). Carries the error text.
    MissingOlean(String),
}

/// One competing declaration for a genuinely ambiguous name. `namespace` is the
/// disambiguator the worker provides (`namespace_name`); source-snapshot
/// candidates have no loaded module, so there is nothing finer to show.
pub(crate) struct CompetingDecl {
    pub name: String,
    pub namespace: Option<String>,
}

/// Recognise an infrastructure failure that is really a missing/stale build
/// artifact rather than a Lean-domain error. Text-coupled heuristic; the only
/// one left, for the env-based query path the worker does not type. See module
/// docs.
pub(crate) fn missing_olean_failure(err: &ServerError) -> bool {
    let text = err.to_string().to_lowercase();
    (text.contains(".olean")
        && (text.contains("does not exist") || text.contains("no such file") || text.contains("object file")))
        || text.contains("unknown module")
        || text.contains("unknown import")
        || text.contains("module not found")
}

/// The wire status token for a degraded, incomplete-build verdict. One producer
/// so every tool agrees on the spelling.
pub(crate) const NEEDS_BUILD_STATUS: &str = "needs_build";

/// The canonical `(warning, next_action)` pair for an incomplete build. Pure so
/// it can be unit-tested without a [`Response`]. `project_root` comes from the
/// response freshness.
pub(crate) fn needs_build_text(project_root: &str, cause: &IncompleteCause) -> (String, String) {
    let detail = match cause {
        IncompleteCause::MissingImports(missing) if !missing.is_empty() => {
            format!(" (missing imports: {})", missing.join(", "))
        }
        IncompleteCause::MissingImports(_) => String::new(),
        IncompleteCause::MissingOlean(text) => format!(" (blocked: {})", first_line(text)),
    };
    let warning = format!(
        "the project may not be fully built, so this query ran against an incomplete environment. \
         Run `lake build` in {project_root} and resolve errors, then retry.{detail}"
    );
    let next_action = match cause {
        IncompleteCause::MissingImports(missing) if !missing.is_empty() => {
            format!("lake build {} # then retry", missing.join(" "))
        }
        IncompleteCause::MissingImports(_) | IncompleteCause::MissingOlean(_) => {
            "lake build # complete the project environment, then retry".to_owned()
        }
    };
    (warning, next_action)
}

/// Attach the canonical incomplete-build warning + `lake build` next action.
/// The single rendering point for the condition across all tools.
#[must_use]
pub(crate) fn warn_needs_build<T>(resp: Response<T>, cause: &IncompleteCause) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    let (warning, next_action) = needs_build_text(&resp.freshness.project_root, cause);
    resp.warn(warning).hint(next_action)
}

/// The canonical `(warning, next_action)` pair for a genuinely ambiguous name.
/// Pure for testing. `candidates` must be non-empty.
pub(crate) fn ambiguous_text(candidates: &[CompetingDecl]) -> (String, String) {
    let listed = candidates
        .iter()
        .map(|c| match &c.namespace {
            Some(ns) if !ns.is_empty() => format!("{} (namespace {ns})", c.name),
            _ => c.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    (
        format!("name is genuinely ambiguous; competing declarations: {listed}"),
        "fully-qualify the name to one of the competing declarations".to_owned(),
    )
}

/// Attach the genuine-ambiguity warning naming the competitors + a disambiguate
/// next action. No-op when `candidates` is empty (nothing actionable to say).
#[must_use]
pub(crate) fn warn_ambiguous<T>(resp: Response<T>, candidates: &[CompetingDecl]) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    if candidates.is_empty() {
        return resp;
    }
    let (warning, next_action) = ambiguous_text(candidates);
    resp.warn(warning).hint(next_action)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_olean_failure_recognizes_build_artifacts_not_domain_errors() {
        assert!(missing_olean_failure(&ServerError::Lean(
            "object file '/tmp/Missing/Import.olean' does not exist".to_owned()
        )));
        assert!(missing_olean_failure(&ServerError::Lean(
            "unknown module 'Definitely.Missing'".to_owned()
        )));
        assert!(!missing_olean_failure(&ServerError::Lean(
            "unknown declaration Foo.bar".to_owned()
        )));
    }

    #[test]
    fn needs_build_text_names_lake_build_and_avoids_the_word_ambiguous() {
        let (warning, next_action) = needs_build_text("/work/proj", &IncompleteCause::MissingImports(Vec::new()));
        assert!(warning.contains("lake build"));
        assert!(warning.contains("/work/proj"));
        assert!(!warning.to_lowercase().contains("ambiguous"));
        assert!(next_action.contains("lake build"));
    }

    #[test]
    fn needs_build_text_lists_missing_imports_in_warning_and_action() {
        let cause = IncompleteCause::MissingImports(vec!["Foo.Bar".to_owned(), "Baz".to_owned()]);
        let (warning, next_action) = needs_build_text("/work/proj", &cause);
        assert!(warning.contains("Foo.Bar"));
        assert!(warning.contains("missing imports"));
        assert!(next_action.contains("lake build Foo.Bar Baz"));
    }

    #[test]
    fn needs_build_text_includes_the_blocking_olean_for_missing_olean() {
        let cause = IncompleteCause::MissingOlean("object file '/p/Foo.olean' does not exist\nsecond line".to_owned());
        let (warning, _) = needs_build_text("/work/proj", &cause);
        assert!(warning.contains("Foo.olean"));
        assert!(!warning.contains("second line"));
    }

    #[test]
    fn ambiguous_text_names_each_competitor_with_its_namespace() {
        let candidates = vec![
            CompetingDecl {
                name: "boundary.isPushout".to_owned(),
                namespace: Some("SSet".to_owned()),
            },
            CompetingDecl {
                name: "boundary.isPushout".to_owned(),
                namespace: None,
            },
        ];
        let (warning, next_action) = ambiguous_text(&candidates);
        assert!(warning.contains("ambiguous"));
        assert!(warning.contains("namespace SSet"));
        assert!(next_action.contains("fully-qualify"));
    }
}
