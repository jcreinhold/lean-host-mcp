//! The host artifacts `install` built for a worker, read before the worker
//! opens.
//!
//! The worker's readiness mode checks its shims under shared locks and prints
//! their descriptor; it builds nothing. The server never builds shims: when
//! they are missing or stale, the open fails with the install command.

use std::path::Path;
use std::time::Duration;

use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use lean_toolchain::shared_writer::WriterDisposition;

use super::ProjectBroker;
use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{Result, ServerError};

/// The stage a failed readiness check names in its `target_check`.
const STAGE: &str = "host_artifact_readiness";

/// Ask `worker` for the host artifacts installed for it, by the deadline.
/// Hold no project Arc, opening Arc, capacity turn or worker slot here.
pub(super) async fn installed_host_artifacts(
    root: &Path,
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    deadline: tokio::time::Instant,
    cancellation: &CallCancellation,
) -> Result<LeanWorkerPreparedHostArtifacts> {
    remaining(root, deadline, cancellation)?;
    let mut on_drop = cancellation.cancel_on_drop();
    let cancellation = cancellation.clone();
    let worker = worker.to_path_buf();
    let sysroot = sysroot.to_path_buf();
    let writer = writer.to_path_buf();
    let root = root.to_owned();
    // Keep evidence on the blocking owner, including after a dropped await.
    let operation = cancellation.begin_operation();
    let task = tokio::task::spawn_blocking(move || {
        // The blocking pool can queue this closure after the async admission check.
        let budget = match remaining(&root, deadline, &cancellation) {
            Ok(budget) => budget,
            Err(error) => {
                operation.not_started();
                return Err(error);
            }
        };
        match crate::host_preparation::probe_ready(&worker, &sysroot, &writer, budget, &|| cancellation.is_cancelled())
        {
            Ok(Some(artifacts)) => {
                operation.returned();
                Ok(artifacts)
            }
            Ok(None) => {
                operation.returned();
                Err(ServerError::BadProject(format!(
                    "the shims installed for {} are missing or out of date; run: {}",
                    worker.display(),
                    crate::toolchain::install_fix(&root)
                )))
            }
            Err(failure) => {
                if failure.disposition == WriterDisposition::NotStarted {
                    operation.not_started();
                } else {
                    operation.unknown();
                }
                let target_check = TargetCheckNotStarted {
                    file_label: root.to_string_lossy().into_owned(),
                    stage: STAGE.into(),
                    disposition: "not_started".into(),
                };
                cancellation.target_check_not_started(target_check.clone());
                tracing::debug!(
                    readiness_child_stopped = failure.preparation_child_stopped,
                    "host artifact readiness check failed"
                );
                Err(ServerError::PreparationFailed {
                    message: failure.message,
                    target_check,
                })
            }
        }
    });
    let result = task
        .await
        .map_err(|err| ServerError::Internal(format!("host artifact readiness task failed: {err}")))?;
    on_drop.disarm();
    result
}

fn remaining(root: &Path, deadline: tokio::time::Instant, cancellation: &CallCancellation) -> Result<Duration> {
    if cancellation.is_cancelled() {
        return Err(ProjectBroker::pool_unavailable(root, "request_cancelled", false));
    }
    let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
    if budget.is_zero() {
        return Err(ProjectBroker::pool_unavailable(root, "project_admission_timeout", true));
    }
    Ok(budget)
}

#[cfg(test)]
mod tests;
