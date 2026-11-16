//! Runtime-free preparation outside actor admission and an owned bounded drain.

use std::path::Path;
use std::time::Duration;

use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use lean_toolchain::shared_writer::WriterDisposition;

use super::{ProjectBroker, RuntimeAction, RuntimeControlRequest, RuntimeControlStatus};
use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{Result, ServerError};

const LEASE_SECONDS: u64 = 300;
const DRAIN_POLL: Duration = Duration::from_millis(100);
const RENEW_TICK: Duration = Duration::from_secs(30);

/// Only this exact token can advance or release this preparation's drain.
struct PreparationLease<'a> {
    broker: &'a ProjectBroker,
    token: String,
}

impl PreparationLease<'_> {
    fn renew(&self, root: &Path) -> Result<bool> {
        let response = self.broker.runtime_control(&RuntimeControlRequest {
            action: RuntimeAction::Renew,
            lease_id: Some(self.token.clone()),
            lease_seconds: Some(LEASE_SECONDS),
            retire_idle: false,
        });
        if matches!(
            response.status,
            RuntimeControlStatus::Ok | RuntimeControlStatus::Draining
        ) {
            Ok(response.pool.workers_stopped)
        } else {
            Err(ProjectBroker::pool_unavailable(root, "preparation_lease_lost", true))
        }
    }
}

impl Drop for PreparationLease<'_> {
    fn drop(&mut self) {
        drop(self.broker.runtime_control(&RuntimeControlRequest {
            action: RuntimeAction::Release,
            lease_id: Some(self.token.clone()),
            ..RuntimeControlRequest::default()
        }));
    }
}

#[derive(Clone, Copy)]
enum HelperMode {
    Ready,
    Prepare,
}

impl HelperMode {
    fn stage(self) -> &'static str {
        match self {
            Self::Ready => "host_artifact_readiness",
            Self::Prepare => "host_artifact_preparation",
        }
    }
}

impl ProjectBroker {
    /// No project Arc, opening Arc, capacity turn or worker slot may be held here.
    pub(super) async fn prepare_host_artifacts(
        &self,
        root: &Path,
        worker: &Path,
        sysroot: &Path,
        writer: &Path,
        deadline: tokio::time::Instant,
        cancellation: &CallCancellation,
    ) -> Result<LeanWorkerPreparedHostArtifacts> {
        if let Some(artifacts) = run_helper(
            root,
            worker,
            sysroot,
            writer,
            deadline,
            cancellation,
            HelperMode::Ready,
            None,
        )
        .await?
        {
            return Ok(artifacts);
        }

        let lease = self.preparation_drain(root, deadline, cancellation).await?;
        // Another preparation may have completed between our first probe and
        // owning the pause. Recheck under the owned drain before any writer.
        let result = match run_helper(
            root,
            worker,
            sysroot,
            writer,
            deadline,
            cancellation,
            HelperMode::Ready,
            Some(&lease),
        )
        .await?
        {
            Some(artifacts) => Ok(artifacts),
            None => run_helper(
                root,
                worker,
                sysroot,
                writer,
                deadline,
                cancellation,
                HelperMode::Prepare,
                Some(&lease),
            )
            .await?
            .ok_or_else(|| ServerError::Internal("preparation produced no artifacts".into())),
        };
        // Admission must resume before an opening Arc or slot can be acquired.
        drop(lease);
        result
    }

    async fn preparation_drain(
        &self,
        root: &Path,
        deadline: tokio::time::Instant,
        cancellation: &CallCancellation,
    ) -> Result<PreparationLease<'_>> {
        remaining(root, deadline, cancellation)?;
        let response = self.runtime_control(&RuntimeControlRequest {
            action: RuntimeAction::Drain,
            lease_seconds: Some(LEASE_SECONDS),
            retire_idle: true,
            ..RuntimeControlRequest::default()
        });
        let token = match response.status {
            RuntimeControlStatus::Acquired | RuntimeControlStatus::Draining => response
                .lease_id
                .ok_or_else(|| ServerError::Internal("preparation drain returned no token".into()))?,
            RuntimeControlStatus::Leased => {
                return Err(Self::pool_unavailable(root, "worker_pool_draining", true));
            }
            RuntimeControlStatus::Ok
            | RuntimeControlStatus::Busy
            | RuntimeControlStatus::InvalidLease
            | RuntimeControlStatus::InvalidRequest => {
                return Err(Self::pool_unavailable(root, "preparation_drain_unavailable", true));
            }
        };
        let lease = PreparationLease { broker: self, token };
        if response.pool.workers_stopped {
            return Ok(lease);
        }
        loop {
            remaining(root, deadline, cancellation)?;
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Self::pool_unavailable(root, "request_cancelled", false)),
                () = tokio::time::sleep_until(deadline) => return Err(Self::pool_unavailable(root, "project_admission_timeout", true)),
                () = tokio::time::sleep(DRAIN_POLL) => {},
            }
            if lease.renew(root)? {
                return Ok(lease);
            }
        }
    }
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

#[allow(
    clippy::too_many_arguments,
    reason = "one private preparation boundary binds caller, paths and the owned lease"
)]
async fn run_helper(
    root: &Path,
    worker: &Path,
    sysroot: &Path,
    writer: &Path,
    deadline: tokio::time::Instant,
    cancellation: &CallCancellation,
    mode: HelperMode,
    lease: Option<&PreparationLease<'_>>,
) -> Result<Option<LeanWorkerPreparedHostArtifacts>> {
    let budget = remaining(root, deadline, cancellation)?;
    if let Some(lease) = lease {
        lease.renew(root)?;
    }
    let mut on_drop = cancellation.cancel_on_drop();
    let cancellation = cancellation.clone();
    let worker = worker.to_path_buf();
    let sysroot = sysroot.to_path_buf();
    let writer = writer.to_path_buf();
    let file_label = root.to_string_lossy().into_owned();
    // Keep evidence on the blocking owner, including after a dropped await.
    let operation = cancellation.begin_operation();
    let mut task = tokio::task::spawn_blocking(move || {
        let result = match mode {
            HelperMode::Ready => crate::host_preparation::probe_ready(&worker, &sysroot, &writer, budget, &|| {
                cancellation.is_cancelled()
            }),
            HelperMode::Prepare => {
                crate::host_preparation::prepare_cancellable(&worker, &sysroot, &writer, budget, &|| {
                    cancellation.is_cancelled()
                })
                .map(Some)
            }
        };
        match result {
            Ok(artifacts) => {
                operation.returned();
                Ok(artifacts)
            }
            Err(failure) => {
                if failure.disposition == WriterDisposition::NotStarted {
                    operation.not_started();
                } else {
                    operation.unknown();
                }
                let target_check = TargetCheckNotStarted {
                    file_label,
                    stage: mode.stage().into(),
                    disposition: "not_started".into(),
                };
                cancellation.target_check_not_started(target_check.clone());
                tracing::debug!(
                    preparation_child_stopped = failure.preparation_child_stopped,
                    "runtime-free host helper failed; independent writer closure is not implied"
                );
                Err(ServerError::PreparationFailed {
                    message: failure.message,
                    target_check,
                })
            }
        }
    });
    let result = loop {
        tokio::select! {
            result = &mut task => break result.map_err(|err| ServerError::Internal(format!("host preparation task failed: {err}")))?,
            () = tokio::time::sleep(RENEW_TICK), if lease.is_some() => {
                if let Some(lease) = lease {
                    lease.renew(root)?;
                }
            }
        }
    };
    on_drop.disarm();
    result
}

#[cfg(test)]
mod tests;
