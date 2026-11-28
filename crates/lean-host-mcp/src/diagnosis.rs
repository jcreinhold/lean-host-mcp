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
//! *exposed* (the agent must build with `lean_build`), never masked. An import
//! that names no module is not a build-state problem: no build can supply it,
//! so its rows and next action say to fix the import instead.
//!
//! As of `lean-rs-worker-protocol` 0.1.18 the worker classifies resolution at
//! its own boundary: `proof_state` and `verify_declaration` return typed
//! `NeedsBuild` / `Ambiguous { candidates }` verdicts, so the parent no longer
//! string-matches those. A file check whose imports Lake reports out of date
//! arrives typed too, as [`ServerError::OutOfDate`], and an import Lake finds
//! no module for as [`ServerError::UnknownImports`]. The one remaining text
//! heuristic, in [`missing_olean_failure`], covers the env-based query path
//! (`find_references`, `search_for_proof`), where an unbuilt transitive
//! dependency still surfaces as an infrastructure [`ServerError`].

use schemars::JsonSchema;
use serde::Serialize;

use crate::envelope::{Response, RuntimeFacts, RuntimeRestartEvent};
use crate::error::{Result, ServerError};
use crate::trust::{ArtifactTrust, build_action};

/// Why a query ran against an environment that was not fully built. Each
/// variant carries only what the renderer needs to name the blocking work.
pub(crate) enum IncompleteCause {
    /// Modules the open environment did not have (worker `MissingImports`).
    MissingImports(Vec<String>),
    /// A query hit a missing `.olean` (an infrastructure [`ServerError`] whose
    /// text [`missing_olean_failure`] recognised). Carries the error text.
    MissingOlean(String),
    /// Targets Lake must rebuild before it can set up the file
    /// ([`ServerError::OutOfDate`]).
    OutOfDate(Vec<String>),
    /// Imports that name no module ([`ServerError::UnknownImports`]): no build
    /// can supply them, so the caller must fix the import.
    UnknownImports(Vec<String>),
}

impl IncompleteCause {
    /// The cause a build-state failure names: Lake's out-of-date targets or
    /// unknown imports when it reported them, else the error text.
    pub(crate) fn from_error(err: &ServerError) -> Self {
        if let ServerError::OutOfDate { targets, .. } = err {
            Self::OutOfDate(targets.clone())
        } else if let ServerError::UnknownImports { modules, .. } = err {
            Self::UnknownImports(modules.clone())
        } else {
            Self::MissingOlean(err.to_string())
        }
    }
}

/// One competing declaration for a genuinely ambiguous name. `namespace` is the
/// disambiguator the worker provides (`namespace_name`); source-snapshot
/// candidates have no loaded module, so there is nothing finer to show.
pub(crate) struct CompetingDecl {
    pub name: String,
    pub namespace: Option<String>,
}

/// Recognise an infrastructure failure that is really a missing/stale build
/// artifact rather than a Lean-domain error: Lake's typed out-of-date or
/// unknown-import verdict, or text naming a missing `.olean`. The text match
/// is the only heuristic left, for the env-based query path the worker does
/// not type. See module docs.
pub(crate) fn missing_olean_failure(err: &ServerError) -> bool {
    if matches!(err, ServerError::OutOfDate { .. } | ServerError::UnknownImports { .. }) {
        return true;
    }
    let text = err.to_string().to_lowercase();
    (text.contains(".olean")
        && (text.contains("does not exist") || text.contains("no such file") || text.contains("object file")))
        || text.contains("unknown module")
        || text.contains("unknown import")
        || text.contains("module not found")
}

/// The modules a missing-`.olean` failure names, for a check whose own
/// imports are `imports`: the module of Lean's "object file ... of module M
/// does not exist", or each import under the root of Lean's "unknown module
/// prefix 'P'". Lake can then tell which of them name no module at all.
pub(crate) fn unloaded_modules(text: &str, imports: &[String]) -> Vec<String> {
    if let Some(module) = olean_module(text) {
        return vec![module.to_owned()];
    }
    let Some(prefix) = text
        .split_once("unknown module prefix '")
        .and_then(|(_, rest)| rest.split_once('\''))
        .map(|(prefix, _)| prefix)
    else {
        return Vec::new();
    };
    imports
        .iter()
        .filter(|import| import.split('.').next() == Some(prefix))
        .cloned()
        .collect()
}

/// The module of Lean's "object file 'PATH' of module M does not exist"
/// (`Lean/Environment.lean`), when `text` holds that message.
fn olean_module(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("' of module ")?;
    let (module, _) = rest.split_once(" does not exist")?;
    (!module.is_empty() && !module.contains(char::is_whitespace)).then_some(module)
}

/// The wire status token for a degraded, incomplete-build verdict. One producer
/// so every tool agrees on the spelling.
pub(crate) const NEEDS_BUILD_STATUS: &str = "needs_build";

/// A worker call either produced its value, or it hit an unbuilt dependency
/// while assembling the environment. The second case is recoverable and
/// agent-actionable (build with `lean_build`, or fix an import that names no
/// module), not an infrastructure failure, so the tools degrade it into a
/// `needs_build` verdict rather than letting it propagate as an MCP transport
/// error.
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
    let (detail, modules): (String, Vec<&str>) = match cause {
        IncompleteCause::UnknownImports(modules) => return unknown_import_text(project_root, modules),
        IncompleteCause::MissingImports(missing) if !missing.is_empty() => (
            format!(" (missing imports: {})", missing.join(", ")),
            missing.iter().map(String::as_str).collect(),
        ),
        IncompleteCause::OutOfDate(targets) if !targets.is_empty() => (
            format!(" (out of date: {})", targets.join(", ")),
            targets.iter().map(String::as_str).collect(),
        ),
        IncompleteCause::MissingImports(_) | IncompleteCause::OutOfDate(_) => (String::new(), Vec::new()),
        IncompleteCause::MissingOlean(text) => (
            format!(" (blocked: {})", first_line(text)),
            olean_module(text).into_iter().collect(),
        ),
    };
    let warning = format!(
        "the project at {project_root} may not be fully built, so this query ran against an incomplete \
         environment. Build it with lean_build, resolve errors, then retry.{detail}"
    );
    (warning, build_action(&modules))
}

/// The `(warning, next_action)` pair for imports that name no module.
fn unknown_import_text(project_root: &str, modules: &[String]) -> (String, String) {
    let named = modules
        .iter()
        .map(|module| format!("`{module}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let (subject, action) = if named.is_empty() {
        (
            "for an import".to_owned(),
            "fix the import Lake finds no module for, then retry".to_owned(),
        )
    } else {
        (
            format!("named {named}"),
            format!("fix the import of {named}, then retry"),
        )
    };
    (
        format!(
            "Lake finds no module {subject} for the project at {project_root}, so no build can supply it. \
             Fix the import, then retry."
        ),
        action,
    )
}

/// Attach the canonical incomplete-build warning and next action: a
/// `lean_build` call, or an import fix when an import names no module. The
/// single rendering point for the condition across all tools.
#[must_use]
pub(crate) fn warn_needs_build<T>(resp: Response<T>, cause: &IncompleteCause) -> Response<T>
where
    T: Serialize + JsonSchema,
{
    let (warning, next_action) = needs_build_text(&resp.freshness.project_root, cause);
    resp.with_trust_artifacts(needs_build_trust_artifacts(cause))
        .warn(warning)
        .hint(next_action)
}

fn needs_build_trust_artifacts(cause: &IncompleteCause) -> Vec<ArtifactTrust> {
    match cause {
        IncompleteCause::MissingImports(missing) if missing.is_empty() => {
            vec![ArtifactTrust::olean_project_missing_build(
                "project environment is missing built .olean artifacts",
            )]
        }
        IncompleteCause::MissingImports(missing) => missing
            .iter()
            .cloned()
            .map(ArtifactTrust::olean_module_missing_build)
            .collect(),
        IncompleteCause::MissingOlean(text) => vec![olean_module(text).map_or_else(
            || ArtifactTrust::olean_project_missing_build(first_line(text)),
            ArtifactTrust::olean_module_missing_build,
        )],
        IncompleteCause::UnknownImports(modules) => modules
            .iter()
            .cloned()
            .map(ArtifactTrust::olean_module_unknown_import)
            .collect(),
        IncompleteCause::OutOfDate(targets) if targets.is_empty() => {
            vec![ArtifactTrust::olean_project_stale_build(
                "Lake reports an import out of date",
            )]
        }
        IncompleteCause::OutOfDate(targets) => targets
            .iter()
            .cloned()
            .map(ArtifactTrust::olean_module_stale_build)
            .collect(),
    }
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
/// Returns a disrupting final-attempt `call_restart`, or an earlier attempt's
/// failure unless the controller explicitly recorded a completed retry. A
/// completed retry does not excuse a new failure in its own attempt.
/// `max_requests` / `max_imports` / `import_residue` /
/// `import_residue_idle` / `import_accounting_unknown` / `idle` / `explicit` are excluded: they are planned
/// hygiene cycles, not duress. A residue cycle in particular runs *before* the
/// import that would have crossed the budget, so the call it precedes runs in a
/// fresh child under no pressure at all — the opposite of the RSS causes below,
/// which fire only once pressure is already being felt.
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
/// This server no longer configures either RSS threshold — one Lean *heap*
/// budget replaced them — so `rss_post_job` and `rss_hard_limit_exceeded`
/// cannot fire under the shipped policy. They stay classified because
/// `restart_cause_from_worker` still maps the upstream causes, and a
/// classifier that silently reclassifies a cause it stops expecting is worse
/// than one that keeps a quiet arm.
///
/// The cause strings mirror `crate::project::restart::RestartCause::as_str`; the unit
/// test below pins the disrupting set so a new cause there is a conscious
/// decision here.
pub(crate) fn execution_taint(runtime: &RuntimeFacts) -> Option<&RuntimeRestartEvent> {
    runtime
        .call_restart
        .as_ref()
        .filter(|event| restart_disrupts_result(event))
        .or_else(|| {
            (runtime.final_attempt != crate::envelope::FinalAttemptStatus::Completed)
                .then_some(runtime.previous_attempt_failure.as_ref())
                .flatten()
        })
}

pub(crate) fn restart_disrupts_result(event: &RuntimeRestartEvent) -> bool {
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
        "the worker did not complete this call reliably ({cause}){rss}; its result is not trustworthy.",
        cause = event.cause,
    );
    let next_action =
        "retry; if it persists, check the file with lean_status(kind=\"file_diagnostics\") or build its module with lean_build"
            .to_owned();
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
    fn needs_build_text_names_lean_build_not_lake_and_avoids_the_word_ambiguous() {
        let (warning, next_action) = needs_build_text("/work/proj", &IncompleteCause::MissingImports(Vec::new()));
        assert!(warning.contains("lean_build"));
        assert!(warning.contains("/work/proj"));
        assert!(!warning.to_lowercase().contains("ambiguous"));
        assert_eq!(next_action, "run lean_build() to build the default targets, then retry");
        for cause in [
            IncompleteCause::MissingImports(vec!["A".to_owned()]),
            IncompleteCause::OutOfDate(Vec::new()),
            IncompleteCause::MissingOlean("unknown module prefix 'P'".to_owned()),
            IncompleteCause::UnknownImports(vec!["A".to_owned()]),
        ] {
            let (warning, next_action) = needs_build_text("/work/proj", &cause);
            assert!(
                !format!("{warning} {next_action}").contains("lake "),
                "{warning} {next_action}"
            );
        }
    }

    #[test]
    fn needs_build_text_lists_missing_imports_in_warning_and_action() {
        let cause = IncompleteCause::MissingImports(vec!["Foo.Bar".to_owned(), "Baz".to_owned()]);
        let (warning, next_action) = needs_build_text("/work/proj", &cause);
        assert!(warning.contains("Foo.Bar"));
        assert!(warning.contains("missing imports"));
        assert_eq!(
            next_action,
            r#"run lean_build(targets=["+Foo.Bar","+Baz"]), then retry"#
        );
    }

    #[test]
    fn needs_build_text_includes_the_blocking_olean_for_missing_olean() {
        let cause = IncompleteCause::MissingOlean("object file '/p/Foo.olean' does not exist\nsecond line".to_owned());
        let (warning, _) = needs_build_text("/work/proj", &cause);
        assert!(warning.contains("Foo.olean"));
        assert!(!warning.contains("second line"));
    }

    #[test]
    fn a_missing_olean_that_names_its_module_builds_that_module() {
        use crate::trust::{TrustScope, TrustStatus};
        let text = "worker unavailable: worker returned lean_rs.lean_exception: lean-rs: Lean threw UserError: \
                    object file '/p/.lake/build/lib/lean/Chain/D.olean' of module Chain.D does not exist";
        let cause = IncompleteCause::MissingOlean(text.to_owned());
        assert_eq!(
            needs_build_text("/p", &cause).1,
            r#"run lean_build(targets=["+Chain.D"]), then retry"#
        );
        let rows = needs_build_trust_artifacts(&cause);
        assert_eq!(
            rows.iter()
                .map(|row| (row.scope, row.status, row.module.as_deref()))
                .collect::<Vec<_>>(),
            [(TrustScope::Module, TrustStatus::MissingBuild, Some("Chain.D"))]
        );
    }

    #[test]
    fn unknown_imports_say_to_fix_the_import_with_one_row_each() {
        use crate::trust::{ArtifactKind, TrustScope, TrustStatus};
        let err = ServerError::UnknownImports {
            modules: vec!["Chain.Typo".to_owned(), "Nope.Typo".to_owned()],
            output: "error: unknown module `Nope.Typo`".to_owned(),
        };
        assert!(missing_olean_failure(&err));
        let cause = IncompleteCause::from_error(&err);
        let (warning, next_action) = needs_build_text("/work/proj", &cause);
        assert!(
            warning.contains("no module named `Chain.Typo`, `Nope.Typo`"),
            "{warning}"
        );
        assert!(!warning.contains("lean_build"), "{warning}");
        assert_eq!(next_action, "fix the import of `Chain.Typo`, `Nope.Typo`, then retry");
        assert_eq!(
            needs_build_trust_artifacts(&cause)
                .into_iter()
                .map(|row| (row.artifact, row.scope, row.status, row.module))
                .collect::<Vec<_>>(),
            ["Chain.Typo", "Nope.Typo"].map(|module| (
                ArtifactKind::Olean,
                TrustScope::Module,
                TrustStatus::UnknownImport,
                Some(module.to_owned())
            ))
        );
    }

    #[test]
    fn unloaded_modules_read_the_module_lean_could_not_load() {
        let imports = ["Nope.Typo", "Nope", "Nopes.X", "Chain.A"].map(str::to_owned);
        // `Lean/Environment.lean`: an import under a known search-path root.
        assert_eq!(
            unloaded_modules(
                "lean-rs: Lean threw UserError: object file '/p/Chain/Typo.olean' of module Chain.Typo does not exist",
                &imports
            ),
            ["Chain.Typo"]
        );
        // `Lean/Util/Path.lean`: only the root is named, so every import under
        // it is a candidate, and no other.
        assert_eq!(
            unloaded_modules(
                "unknown module prefix 'Nope'\n\nNo directory 'Nope' or file 'Nope.olean' in the search path entries:\n/p",
                &imports
            ),
            ["Nope.Typo", "Nope"]
        );
        assert!(unloaded_modules("worker thread gone", &imports).is_empty());
        assert_eq!(olean_module("object file 'x' of module  does not exist"), None);
    }

    #[test]
    fn out_of_date_setup_is_needs_build_with_one_stale_row_per_target() {
        use crate::trust::{ArtifactKind, TrustScope, TrustStatus};
        let stale = || ServerError::OutOfDate {
            targets: vec!["A".to_owned(), "B.C".to_owned()],
            output: "error: target is out-of-date and needs to be rebuilt".to_owned(),
        };
        assert!(matches!(
            classify_missing_olean::<()>(Err(stale())),
            Ok(CallOutcome::NeedsBuild(ServerError::OutOfDate { .. }))
        ));
        let cause = IncompleteCause::from_error(&stale());
        let rows = |cause: &IncompleteCause| {
            needs_build_trust_artifacts(cause)
                .into_iter()
                .map(|row| (row.artifact, row.scope, row.status, row.module))
                .collect::<Vec<_>>()
        };
        let stale_module = |module: &str| {
            (
                ArtifactKind::Olean,
                TrustScope::Module,
                TrustStatus::StaleBuild,
                Some(module.to_owned()),
            )
        };
        assert_eq!(rows(&cause), [stale_module("A"), stale_module("B.C")]);
        let (warning, next_action) = needs_build_text("/work/proj", &cause);
        assert!(warning.contains("out of date: A, B.C"), "{warning}");
        assert_eq!(next_action, r#"run lean_build(targets=["+A","+B.C"]), then retry"#);

        assert_eq!(
            rows(&IncompleteCause::OutOfDate(Vec::new())),
            [(ArtifactKind::Olean, TrustScope::Project, TrustStatus::StaleBuild, None)]
        );
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
        // Planned hygiene cycles do not: the cycle was routine.
        for cause in [
            "max_requests",
            "max_imports",
            "import_residue",
            "import_residue_idle",
            "import_accounting_unknown",
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
    fn only_explicit_completed_retry_clears_previous_failure_not_final_restart() {
        use crate::envelope::FinalAttemptStatus;
        let failure = facts_with_restart("child_abort").call_restart;
        for status in [
            FinalAttemptStatus::Unknown,
            FinalAttemptStatus::Interrupted,
            FinalAttemptStatus::Completed,
        ] {
            let mut facts = RuntimeFacts {
                retry_count: 1,
                final_attempt: status,
                previous_attempt_failure: failure.clone(),
                ..RuntimeFacts::default()
            };
            assert_eq!(
                execution_taint(&facts).is_none(),
                status == FinalAttemptStatus::Completed
            );
            facts.call_restart = failure.clone();
            assert!(
                execution_taint(&facts).is_some(),
                "final-attempt abort must never be excused"
            );
        }
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
        assert!(!next_action.contains("lake "), "{next_action}");
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
