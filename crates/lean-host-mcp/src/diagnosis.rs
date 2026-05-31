//! Resolution-health classification: the parent's one honest verdict about
//! whether a project's environment is complete enough to answer a query.
//!
//! Ousterhout ch.10 (exception aggregation). An incomplete project build
//! reaches the parent in several disguises — a worker `MissingImports`
//! outcome, a missing-`.olean` [`ServerError`], a `verification_status` of
//! "ambiguous", and the worker's `"declaration name is ambiguous in the
//! module"` message — and each tool used to present it differently (a hard
//! error here, a misleading "ambiguous" there). This module collapses them
//! into one [`IncompleteCause`] plus one renderer ([`warn_needs_build`]) so
//! every tool emits the *same* actionable wording. Per ch.10.10 the
//! build-state is deliberately *exposed* (the agent must learn to run
//! `lake build`), never masked.
//!
//! ## Why no genuine-ambiguity verdict here
//!
//! A genuine multi-candidate ambiguity would be actionable (name the
//! competitors). But on the current worker (`lean-rs-worker-protocol` 0.1.x)
//! no live tool path can observe one: `verify_declaration`'s `Ambiguous`
//! status carries no candidates, and the only candidate-bearing shape
//! (`DeclarationTargetResult::Ambiguous`) is reached through a selector no
//! tool issues. Every "ambiguous" the parent can actually see is therefore a
//! zero-candidate verdict, which is the incomplete-build condition wearing the
//! wrong name. Surfacing genuine ambiguity with its competitors is a
//! worker-side change tracked in `~/Code/prompts/lean-rs/12-*.md`.
//!
//! ## Fragile string matches are quarantined here
//!
//! [`missing_olean_failure`] and [`signals_unresolved`] match on worker
//! message text. They are worker-version-coupled heuristics, used only where
//! no structured discriminant exists, and live in this one module so the
//! coupling has a single home. The proper fix is a typed worker outcome
//! (same proposal).

use schemars::JsonSchema;
use serde::Serialize;

use crate::envelope::Response;
use crate::error::ServerError;

/// Why a query ran against an environment that was not fully built. Each
/// variant carries only what the renderer needs to name the blocking work.
pub(crate) enum IncompleteCause {
    /// The session's requested imports named modules the open environment did
    /// not have (worker `MissingImports.missing`).
    MissingImports(Vec<String>),
    /// A query hit a missing `.olean` (an infrastructure [`ServerError`] whose
    /// text [`missing_olean_failure`] recognised). Carries the error text.
    MissingOlean(String),
    /// A name did not resolve uniquely with no competing candidates to show —
    /// on the current worker this is the incomplete-build condition reported
    /// as "ambiguous" (see module docs).
    Unresolved,
}

/// Recognise an infrastructure failure that is really a missing/stale build
/// artifact rather than a Lean-domain error. Text-coupled heuristic; see the
/// module docs.
pub(crate) fn missing_olean_failure(err: &ServerError) -> bool {
    let text = err.to_string().to_lowercase();
    (text.contains(".olean")
        && (text.contains("does not exist") || text.contains("no such file") || text.contains("object file")))
        || text.contains("unknown module")
        || text.contains("unknown import")
        || text.contains("module not found")
}

/// Recognise the worker's name-resolution message that, with no candidates to
/// show, means the project environment is incomplete. Text-coupled heuristic;
/// see the module docs.
pub(crate) fn signals_unresolved(message: &str) -> bool {
    message.to_lowercase().contains("ambiguous in the module")
}

/// The wire status token for a degraded, incomplete-build verdict. One
/// producer so every tool agrees on the spelling.
pub(crate) const NEEDS_BUILD_STATUS: &str = "needs_build";

/// The canonical `(warning, next_action)` pair for an incomplete build. Pure
/// so it can be unit-tested without a [`Response`]. `project_root` comes from
/// the response freshness.
pub(crate) fn needs_build_text(project_root: &str, cause: &IncompleteCause) -> (String, String) {
    let detail = match cause {
        IncompleteCause::MissingImports(missing) if !missing.is_empty() => {
            format!(" (missing imports: {})", missing.join(", "))
        }
        IncompleteCause::MissingImports(_) | IncompleteCause::Unresolved => String::new(),
        IncompleteCause::MissingOlean(text) => format!(" (blocked: {})", first_line(text)),
    };
    let warning = format!(
        "name did not resolve to a unique declaration; the project may not be fully built. \
         Run `lake build` in {project_root} and resolve errors, then retry.{detail}"
    );
    let next_action = match cause {
        IncompleteCause::MissingImports(missing) if !missing.is_empty() => {
            format!("lake build {} # then retry", missing.join(" "))
        }
        IncompleteCause::MissingImports(_) | IncompleteCause::MissingOlean(_) | IncompleteCause::Unresolved => {
            "lake build # complete the project environment, then retry".to_owned()
        }
    };
    (warning, next_action)
}

/// Attach the canonical incomplete-build warning + `lake build` next action to
/// a response. The single rendering point for the condition across all tools.
#[must_use]
pub(crate) fn warn_needs_build<T>(resp: Response<T>, cause: &IncompleteCause) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    let (warning, next_action) = needs_build_text(&resp.freshness.project_root, cause);
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

    // The matched strings are coupled to worker message text, confirmed
    // against lean-rs-worker-protocol 0.1.17. Re-validate when bumping
    // lean-rs-worker-parent; a wording change makes these silently fall back.
    #[test]
    fn signals_unresolved_matches_only_the_ambiguous_in_module_message() {
        assert!(signals_unresolved("declaration name is ambiguous in the module"));
        assert!(signals_unresolved("Declaration name is AMBIGUOUS IN THE MODULE"));
        assert!(!signals_unresolved("unknown identifier Foo.bar"));
    }

    #[test]
    fn needs_build_text_names_lake_build_and_avoids_the_word_ambiguous() {
        let (warning, next_action) = needs_build_text("/work/proj", &IncompleteCause::Unresolved);
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
}
