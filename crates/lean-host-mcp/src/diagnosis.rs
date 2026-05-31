//! Resolution-health rendering: the parent's one set of honest, actionable
//! messages for the ways a verdict is untrustworthy — the project environment
//! is incomplete (`needs_build`), the name is genuinely ambiguous (multiple
//! competing declarations), or the verdict was computed while the worker was
//! recycled/crashed mid-call (`worker_recycled`, [`execution_taint`]).
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

use crate::envelope::{Response, RuntimeFacts, RuntimeRestartEvent};
use crate::error::{Result, ServerError};

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

/// A worker call either produced its value, or it hit an unbuilt dependency
/// while assembling the environment. The second case is recoverable and
/// agent-actionable (run `lake build`), not an infrastructure failure, so the
/// tools degrade it into a `needs_build` verdict rather than letting it
/// propagate as an MCP transport error.
pub(crate) enum CallOutcome<T> {
    Ready(T),
    NeedsBuild(ServerError),
}

/// Split a worker-call result into the normal value and the
/// "ran against an unbuilt dependency" case. The single chokepoint where
/// `verify_declaration`, `proof_state`, and `try_proof_step` recognise the
/// condition, so all three degrade on exactly the predicate `find_references`
/// already uses ([`missing_olean_failure`]). `find_references` does not call
/// this — its loop skips the file and continues rather than returning early —
/// but it shares the same predicate, so the policy lives in one place.
///
/// # Errors
///
/// Propagates any [`ServerError`] that is *not* a missing-`.olean` failure
/// unchanged; only the recoverable build-state case is captured as
/// [`CallOutcome::NeedsBuild`].
pub(crate) fn classify_missing_olean<T>(outcome: Result<T>) -> Result<CallOutcome<T>> {
    match outcome {
        Ok(value) => Ok(CallOutcome::Ready(value)),
        Err(err) if missing_olean_failure(&err) => Ok(CallOutcome::NeedsBuild(err)),
        Err(err) => Err(err),
    }
}

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

/// The wire status token for a verdict computed while the worker was recycled
/// or crashed mid-call. One producer so every tool agrees on the spelling.
pub(crate) const WORKER_RECYCLED_STATUS: &str = "worker_recycled";

/// Recognise that this call's verdict was computed under infrastructure duress:
/// the worker was recycled or restarted *during* the call by a job-disrupting
/// cause, so a non-positive Lean verdict (a rejection, `not_found`, timeout) is
/// not reliable evidence about the declaration — it may be a casualty of the
/// recycle, not a real result.
///
/// Returns the `call_restart` event for a job-disrupting cause; `None`
/// otherwise. The benign causes are deliberately excluded:
/// - `rss_import_switch` / `import_profile_switch` cycle the worker *before* the
///   job runs, so the job then executes on a fresh worker — its verdict is
///   sound.
/// - `max_requests` / `max_imports` / `idle` / `explicit` are planned hygiene
///   cycles, not duress.
///
/// `rss_post_job` *is* job-disrupting even though it fires after the job
/// returned `Ok`: crossing the post-job RSS budget means the job elaborated
/// under heavy memory pressure, and report 62 §A documented that exact
/// condition degrading a verdict — a *valid* lemma returned `not_found` while
/// the worker sat 2.4 GiB over its 5 GiB cap. The worker returns a degraded
/// value rather than crashing, so "the job returned `Ok`" is not evidence the
/// verdict is sound. A `verified` verdict is never relabeled regardless, so a
/// marginal-overage false positive only costs a retry hint.
///
/// The cause strings mirror `crate::project::RestartCause::as_str`; the unit
/// test below pins the disrupting set so a new cause there is a conscious
/// decision here.
pub(crate) fn execution_taint(runtime: &RuntimeFacts) -> Option<&RuntimeRestartEvent> {
    let event = runtime.call_restart.as_ref()?;
    matches!(
        event.cause.as_str(),
        "rss_post_job"
            | "rss_hard_limit_exceeded"
            | "child_abort"
            | "child_exit"
            | "session_missing"
            | "worker_internal"
            | "timeout"
            | "cancelled"
    )
    .then_some(event)
}

/// The canonical `(warning, next_action)` pair for an execution-tainted verdict.
/// Pure so it can be unit-tested without a [`Response`]. Names the recycle cause
/// and, when known, the RSS numbers that explain it.
pub(crate) fn execution_taint_text(event: &RuntimeRestartEvent) -> (String, String) {
    let rss = match (event.rss_kib, event.limit_kib) {
        (Some(rss), Some(limit)) => format!(" (rss {rss} KiB vs limit {limit} KiB)"),
        (Some(rss), None) => format!(" (rss {rss} KiB)"),
        _ => String::new(),
    };
    let warning = format!(
        "the worker was recycled or restarted during this call ({cause}){rss}; any non-positive outcome here — a \
         rejection, `not_found`, a failed tactic, or empty goals — may be a casualty of the recycle rather than a real \
         result, and is not trustworthy. Retry; if it persists, the module is too heavy for the worker's memory budget \
         (raise LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB or verify with `lake build` / `lake env lean`).",
        cause = event.cause,
    );
    let next_action = "retry; if it persists, verify with `lake build <module>` or `lake env lean <file>`".to_owned();
    (warning, next_action)
}

/// Attach the canonical execution-taint warning + retry next action. The single
/// rendering point for the condition across `verify_declaration`,
/// `try_proof_step`, and `proof_state`.
#[must_use]
pub(crate) fn warn_execution_taint<T>(resp: Response<T>, event: &RuntimeRestartEvent) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    let (warning, next_action) = execution_taint_text(event);
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
    fn classify_missing_olean_routes_only_build_artifacts_to_needs_build() {
        // A missing-`.olean` infrastructure failure becomes the recoverable
        // needs_build case.
        let missing: Result<()> = Err(ServerError::Lean(
            "object file '/p/Dep.olean' of module Dep does not exist".to_owned(),
        ));
        assert!(matches!(
            classify_missing_olean(missing),
            Ok(CallOutcome::NeedsBuild(_))
        ));

        // An unrelated infrastructure error still propagates.
        let other: Result<()> = Err(ServerError::Lean("worker thread gone".to_owned()));
        assert!(classify_missing_olean(other).is_err());

        // A success passes through untouched.
        assert!(matches!(classify_missing_olean(Ok(7)), Ok(CallOutcome::Ready(7))));
    }

    fn facts_with_restart(cause: &str) -> RuntimeFacts {
        RuntimeFacts {
            call_restart: Some(RuntimeRestartEvent {
                cause: cause.to_owned(),
                reason: format!("{cause} current_kib=7000000 limit_kib=5000000"),
                worker_generation: 9,
                planned: false,
                rss_kib: Some(7_000_000),
                limit_kib: Some(5_000_000),
            }),
            ..RuntimeFacts::default()
        }
    }

    #[test]
    fn execution_taint_flags_job_disrupting_causes_only() {
        // Job-disrupting causes taint the verdict.
        for cause in [
            "rss_post_job",
            "rss_hard_limit_exceeded",
            "child_abort",
            "child_exit",
            "session_missing",
            "worker_internal",
            "timeout",
            "cancelled",
        ] {
            assert!(
                execution_taint(&facts_with_restart(cause)).is_some(),
                "{cause} should taint the verdict"
            );
        }
        // A pre-job clean cycle and planned hygiene cycles do not: the job ran
        // on a fresh worker, or the cycle was routine.
        for cause in [
            "rss_import_switch",
            "import_profile_switch",
            "max_requests",
            "max_imports",
            "idle",
            "explicit",
        ] {
            assert!(
                execution_taint(&facts_with_restart(cause)).is_none(),
                "{cause} should not taint the verdict"
            );
        }
        // No restart at all: nothing to flag.
        assert!(execution_taint(&RuntimeFacts::default()).is_none());
    }

    #[test]
    fn execution_taint_text_names_cause_and_rss_and_offers_a_retry() {
        let (warning, next_action) = execution_taint_text(&RuntimeRestartEvent {
            cause: "rss_post_job".to_owned(),
            reason: String::new(),
            worker_generation: 1,
            planned: false,
            rss_kib: Some(7_600_000),
            limit_kib: Some(5_242_880),
        });
        assert!(warning.contains("rss_post_job"));
        assert!(warning.contains("7600000"));
        assert!(warning.contains("5242880"));
        assert!(warning.contains("not trustworthy"));
        assert!(next_action.contains("retry"));
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
