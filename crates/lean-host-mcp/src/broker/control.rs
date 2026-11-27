//! Worker-pool observation for `lean_runtime`, and the activity count it
//! reads.
//!
//! The registry lock is the admission boundary: a caller holds a project Arc
//! before using a resident worker, or an opening-lock Arc before starting one.
//! Preparation and idle maintenance join the activity count before they read
//! options or launch native work, so a snapshot counts them until their owned
//! cleanup ends. A snapshot is advisory: another request can start right after
//! it.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use schemars::JsonSchema;
use serde::Serialize;

use super::{BrokerInner, ProjectBroker};
use crate::envelope::RuntimeFacts;

/// This server's build, settings and worker pool, observed without starting
/// or querying Lean.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RuntimeStatus {
    /// Binary source identity captured when it was built.
    pub build: crate::status::BuildIdentity,
    /// Actual startup-resolved policy, including derived memory limits.
    pub effective_settings: crate::status::EffectiveSettings,
    /// Negotiated extensions; absence on older servers means unsupported.
    pub capabilities: Vec<String>,
    /// Queued and running rows of the request table. Filled by a service that
    /// has the table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<crate::requests::RequestCounts>,
    /// The worker pool at the time of the call.
    pub pool: WorkerPoolStatus,
}

/// Snapshot of worker activity and capacity, without starting or querying Lean.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkerPoolStatus {
    /// Identity of this broker lifetime; changes after a server restart.
    pub instance_id: String,
    /// Configured upper bound on resident, opening, and retiring worker slots.
    pub max_workers: usize,
    pub max_workers_per_project: usize,
    /// Slots still owned, including shutdowns whose exit is not confirmed.
    pub occupied_slots: usize,
    /// Admitted actor and preparation work, including cleanup after caller loss.
    pub outstanding_jobs: usize,
    /// True only after every worker slot and admitted preparation cleanup is released.
    pub workers_stopped: bool,
    /// Roots with admitted callers preparing, starting, or waiting for startup.
    pub acquiring_projects: Vec<String>,
    /// Resident workers.
    pub projects: Vec<WorkerActivity>,
}

/// Activity of one resident worker, including work still cleaning up after cancellation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkerActivity {
    /// Canonical Lake root owned by this worker controller.
    pub project_root: String,
    pub worker_lane: usize,
    /// True only when no admitted caller or actor message can use this worker.
    pub idle: bool,
    /// Messages or background maintenance running or completing cleanup.
    pub outstanding_jobs: usize,
    /// A broker caller holds this worker, including the interval before enqueue.
    pub admitted_caller: bool,
    /// Whether the controller can currently accept worker messages.
    pub healthy: bool,
    /// Last published runtime facts; RSS is an observation, not a fresh measurement.
    pub runtime: RuntimeFacts,
}

pub(super) struct ControlState {
    instance_id: String,
    active_jobs: Arc<AtomicUsize>,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            active_jobs: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// One admitted preparation: a short run, or a file's module setup, that
/// counts in the pool's activity without taking a worker slot. The run's
/// group holds it until the group has exited.
#[derive(Debug)]
pub(crate) struct PreparationActivityGuard {
    active_jobs: Arc<AtomicUsize>,
}

impl Drop for PreparationActivityGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Actor access to the broker's activity count and project turns.
#[derive(Clone, Default)]
pub(crate) struct MaintenanceAccess {
    inner: Weak<parking_lot::Mutex<BrokerInner>>,
    active_jobs: Arc<AtomicUsize>,
}

impl MaintenanceAccess {
    pub(super) fn new(inner: &Arc<parking_lot::Mutex<BrokerInner>>) -> Self {
        let active_jobs = Arc::clone(&inner.lock().control.active_jobs);
        Self {
            inner: Arc::downgrade(inner),
            active_jobs,
        }
    }

    /// The expected ABI exists before any worker and is never learned from one.
    pub(crate) fn native_contract(
        toolchain: &str,
    ) -> Result<Option<crate::runtime_compatibility::ExpectedContract>, String> {
        let expected = crate::runtime_compatibility::session_expectation(toolchain, None)?;
        crate::runtime_compatibility::ExpectedContract::new(expected.checked_host_bindings).map(Some)
    }

    pub(crate) fn bind_native_contract(
        &self,
        toolchain: &str,
        actual: &crate::runtime_compatibility::ExpectedContract,
    ) -> Result<(String, crate::runtime_compatibility::ExpectedContract), String> {
        let compiled = Self::native_contract(toolchain)?.ok_or("compiled host contract missing")?;
        if actual != &compiled {
            return Err("loaded host ABI differs from compiled requirements".into());
        }
        let inner = self
            .inner
            .upgrade()
            .ok_or("broker lifetime ended before compatibility binding")?;
        let state = inner.lock();
        Ok((state.control.instance_id.clone(), compiled))
    }

    pub(crate) fn activity_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active_jobs)
    }

    pub(crate) fn try_begin(&self, root: &Path, session_id: &str) -> Option<MaintenanceGuard> {
        let inner = self.inner.upgrade()?;
        let mut registry = inner.lock();
        // Idle work never waits for a turn. It runs only when no writer holds
        // or waits for the project.
        let lock = registry.locks.get(root);
        let turn = lock.try_read()?;
        // An evicted/replaced controller cannot start another idle recycle.
        let job = registry
            .registry
            .iter()
            .filter(|(key, _)| key.root == root)
            .find_map(|(_, actor)| actor.begin_maintenance(session_id));
        drop(registry);
        Some(MaintenanceGuard {
            _job: job?,
            _turn: turn,
            builds_finished: lock.builds_finished(),
        })
    }
}

/// Idle work's hold on a project: one active job on the actor and a read
/// turn, so no idle cycle or pre-warm overlaps a write turn.
pub(crate) struct MaintenanceGuard {
    _job: crate::project::ActiveJobGuard,
    _turn: tokio::sync::OwnedRwLockReadGuard<()>,
    builds_finished: u64,
}

impl MaintenanceGuard {
    /// The finished-build count under this read turn.
    pub(crate) const fn builds_finished(&self) -> u64 {
        self.builds_finished
    }
}

impl ProjectBroker {
    /// Count one preparation in the pool's activity, without a worker slot,
    /// until its guard drops.
    pub(crate) fn preparation_activity(&self) -> PreparationActivityGuard {
        let active_jobs = Arc::clone(&self.inner.lock().control.active_jobs);
        active_jobs.fetch_add(1, Ordering::AcqRel);
        PreparationActivityGuard { active_jobs }
    }

    /// This server's identity, as status, compatibility and file receipts name it.
    #[must_use]
    pub fn instance_id(&self) -> String {
        self.inner.lock().control.instance_id.clone()
    }

    /// Observe this server's build, settings and workers. It starts nothing
    /// and reserves nothing.
    #[must_use]
    pub fn runtime_status(&self) -> RuntimeStatus {
        crate::worker_retirement::poll_retirements();
        let pool = self.pool_snapshot(&self.inner.lock());
        RuntimeStatus {
            build: crate::status::BuildIdentity::default(),
            effective_settings: crate::status::EffectiveSettings {
                broker: crate::status::BrokerSettings {
                    max_workers: self.config.max_workers.get(),
                    max_workers_per_project: self.config.max_workers_per_project.get(),
                    idle_timeout_secs: self.config.idle_timeout.as_secs(),
                },
                runtime: crate::status::RuntimeSettings::from(&self.runtime_config),
                server: None,
            },
            capabilities: Vec::new(),
            requests: None,
            pool,
        }
    }

    fn pool_snapshot(&self, inner: &BrokerInner) -> WorkerPoolStatus {
        let occupied_slots = self
            .config
            .max_workers
            .get()
            .saturating_sub(self.worker_slots.available_permits());
        let projects: Vec<_> = inner
            .registry
            .iter()
            .map(|(root, project)| {
                let admitted_caller = Arc::strong_count(project) > 1;
                let outstanding_jobs = project.outstanding_jobs();
                WorkerActivity {
                    project_root: root.root.to_string_lossy().into_owned(),
                    worker_lane: root.lane,
                    idle: outstanding_jobs == 0 && !admitted_caller,
                    outstanding_jobs,
                    admitted_caller,
                    healthy: project.is_healthy(),
                    runtime: project.runtime_facts(),
                }
            })
            .collect();
        let mut acquiring_projects: Vec<_> = inner
            .opening_locks
            .iter()
            .filter(|(_, lock)| lock.strong_count() > 0)
            .map(|(root, _)| root.to_string_lossy().into_owned())
            .collect();
        acquiring_projects.sort();
        let outstanding_jobs = inner.control.active_jobs.load(Ordering::Acquire);
        WorkerPoolStatus {
            instance_id: inner.control.instance_id.clone(),
            max_workers: self.config.max_workers.get(),
            max_workers_per_project: self.config.max_workers_per_project.get(),
            occupied_slots,
            outstanding_jobs,
            workers_stopped: occupied_slots == 0 && acquiring_projects.is_empty() && outstanding_jobs == 0,
            acquiring_projects,
            projects,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn broker() -> Arc<ProjectBroker> {
        ProjectBroker::new(super::super::BrokerConfig {
            lake_root: None,
            max_workers: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            max_lean_runs: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        })
    }

    #[test]
    fn compatibility_contract_is_compiled_before_any_worker_and_cannot_rebind() -> TestResult {
        let broker = broker();
        let access = MaintenanceAccess::new(&broker.inner);
        let original = crate::runtime_compatibility::ExpectedContract::compiled()?;
        assert_eq!(
            MaintenanceAccess::native_contract("leanprover/lean4:v4.35.0")?,
            Some(original.clone())
        );
        let (instance, saved) = access.bind_native_contract("leanprover/lean4:v4.35.0", &original)?;
        assert_eq!(instance, broker.instance_id());
        assert_eq!(saved, original);
        let mut changed = original.clone();
        changed
            .required_host_bindings
            .insert("verify_declaration_batch".into(), "b".repeat(64));
        assert!(access.bind_native_contract("v4.35.0", &changed).is_err());
        assert_eq!(MaintenanceAccess::native_contract("v4.35.0")?, Some(original));
        let status = broker.runtime_status();
        assert_eq!(status.pool.occupied_slots, 0);
        assert_eq!(status.pool.outstanding_jobs, 0);
        assert!(status.pool.projects.is_empty());
        drop(broker);
        // Static requirements authorize no dispatch and remain available during
        // startup/retirement without retaining an otherwise finished broker.
        assert_eq!(MaintenanceAccess::native_contract("v4.35.0")?, Some(saved.clone()));
        assert!(access.bind_native_contract("v4.35.0", &saved).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn a_reserved_slot_keeps_the_pool_from_reporting_stopped() -> TestResult {
        let broker = broker();
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or("deadline out of range")?;
        let retiring = broker
            .reserve_worker_slot(Path::new("/uncertain-retirement"), deadline)
            .await?;
        let held = broker.runtime_status();
        assert_eq!(held.pool.occupied_slots, 1);
        assert!(!held.pool.workers_stopped);
        drop(retiring);
        assert!(broker.runtime_status().pool.workers_stopped);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and a matching installed worker"]
    async fn background_maintenance_and_detached_activity_stay_counted() -> TestResult {
        let root = super::super::tests::worker_fixture();
        let broker = ProjectBroker::new(super::super::BrokerConfig {
            lake_root: Some(root.clone()),
            max_workers: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            max_lean_runs: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        });
        let call = broker
            .admitted_project_runtime(super::super::ProjectHint::Explicit(root), vec![])
            .await?;
        let root = Path::new(&call.freshness.project_root);
        let access = MaintenanceAccess::new(&broker.inner);
        let activity = access
            .try_begin(root, &call.freshness.session_id)
            .ok_or("maintenance was not admitted")?;
        assert_eq!(broker.runtime_status().pool.outstanding_jobs, 1);
        drop(activity);
        let detached = access
            .try_begin(root, &call.freshness.session_id)
            .ok_or("maintenance was not admitted again")?;
        broker.shutdown_all();
        let shut = broker.runtime_status();
        assert!(shut.pool.projects.is_empty());
        assert!(!shut.pool.workers_stopped);
        drop(detached);
        assert!(access.try_begin(root, &call.freshness.session_id).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_detached_startup_counts_until_its_opening_guard_drops() -> TestResult {
        let broker = broker();
        let opening = broker.opening_lock(Path::new("/detached"));
        let guard = Arc::clone(&opening).lock_owned().await;
        drop(opening);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let startup = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let _sent = started_tx.send(());
            let _finished = finish_rx.recv();
        });
        started_rx.await?;
        startup.abort(); // A running blocking task continues after async cancellation.
        let starting = broker.runtime_status();
        assert!(!starting.pool.acquiring_projects.is_empty());
        assert!(!starting.pool.workers_stopped);
        finish_tx.send(())?;
        startup.await?;
        assert!(broker.runtime_status().pool.workers_stopped);
        Ok(())
    }

    #[test]
    fn preparation_activity_counts_without_a_worker_slot() {
        let broker = broker();
        let activity = broker.preparation_activity();
        let busy = broker.runtime_status();
        assert_eq!(busy.pool.occupied_slots, 0);
        assert_eq!(busy.pool.outstanding_jobs, 1);
        assert!(!busy.pool.workers_stopped);
        drop(activity);
        let idle = broker.runtime_status();
        assert_eq!(idle.pool.outstanding_jobs, 0);
        assert!(idle.pool.workers_stopped);
    }
}
