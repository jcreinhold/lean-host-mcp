//! Atomic worker-pool observation and expiring admission leases.
//!
//! The registry lock is the admission boundary: a caller holds a project Arc
//! before using a resident worker, or an opening-lock Arc before starting one.
//! Preparation joins the same activity counter under that registry lock before
//! reading options or launching native work. A lease can therefore be granted
//! only before admission or after all callers and owned cleanup have finished. Network connections are not
//! activity or ownership; disconnecting cannot release somebody else's lease.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{BrokerInner, ProjectBroker};
use crate::envelope::RuntimeFacts;

const DEFAULT_LEASE_SECONDS: u64 = 30;
const MAX_LEASE_SECONDS: u64 = 300;

/// A server-local coordination operation; it never evaluates Lean code.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAction {
    /// Observe current workers without reserving or opening any worker.
    #[default]
    Status,
    /// Pause worker admission only if all admitted requests have finished.
    Acquire,
    /// Pause new admissions while already-admitted work finishes.
    Drain,
    /// Extend an unexpired lease owned by the supplied token.
    Renew,
    /// Resume worker admission for the lease owned by the supplied token.
    Release,
}

/// Request to observe or temporarily pause this server's worker pool.
///
/// Leases are process-local capabilities. Keep their token and renew before
/// expiry while relying on the pause. They do not reserve system RAM or stop
/// external processes. Retiring warm workers is explicitly opt-in.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeControlRequest {
    /// Operation to perform; defaults to a non-mutating snapshot.
    #[serde(default)]
    pub action: RuntimeAction,
    /// Token returned by acquire; required for renew and release.
    #[serde(default)]
    pub lease_id: Option<String>,
    /// Lease lifetime, 1–300 seconds; acquire and renew default to 30 seconds.
    #[serde(default)]
    pub lease_seconds: Option<u64>,
    /// On acquire, explicitly retire idle workers instead of keeping caches warm.
    /// Independent artifact-writer demand may also retire an affected idle cache.
    #[serde(default)]
    pub retire_idle: bool,
}

/// Outcome of one atomic worker-pool coordination operation.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeControlStatus {
    /// Snapshot, renewal, or release succeeded.
    Ok,
    /// The caller owns a live admission lease.
    Acquired,
    /// An owned pause is draining existing work; no external work may start.
    Draining,
    /// A request or startup is already admitted; no lease was taken.
    Busy,
    /// Another unexpired lease already owns admission.
    Leased,
    /// The token is unknown, expired, or from another server instance.
    InvalidLease,
    /// The request contains invalid or inapplicable options.
    InvalidRequest,
}

/// Server-owned outcome plus an observation of its worker pool.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RuntimeControlResult {
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
    /// Result of the requested coordination action.
    pub status: RuntimeControlStatus,
    /// Explanation for a refused or invalid operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Private lease capability, returned only to a successful acquirer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    /// Number of warm workers explicitly asked to retire by this operation.
    pub retirement_requested: usize,
    /// A snapshot is advisory; only an unexpired owned lease fences admission.
    pub pool: WorkerPoolStatus,
}

/// Snapshot of worker activity and capacity, without starting or querying Lean.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkerPoolStatus {
    /// Identity of this broker lifetime; changes after a server restart.
    pub instance_id: String,
    /// Configured upper bound on resident, opening, and retiring worker slots.
    pub max_projects: usize,
    pub max_workers: usize,
    pub max_workers_per_project: usize,
    /// Slots still owned, including shutdowns whose exit is not confirmed.
    pub occupied_slots: usize,
    /// Admitted actor and preparation work, including cleanup after caller loss.
    pub outstanding_jobs: usize,
    /// True only after every worker slot and admitted preparation cleanup is released.
    pub workers_stopped: bool,
    /// Whether an unexpired lease currently prevents new worker admission.
    pub admission_paused: bool,
    /// Remaining lease time, not an assurance that a client owns that lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_remaining_millis: Option<u64>,
    /// Roots with admitted callers preparing, starting, or waiting for startup.
    pub acquiring_projects: Vec<String>,
    /// Current resident projects, in MRU order.
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

struct Lease {
    token: String,
    expires: Instant,
    drain: bool,
    retire_idle: bool,
}

pub(super) struct ControlState {
    instance_id: String,
    lease: Option<Lease>,
    active_jobs: Arc<AtomicUsize>,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            lease: None,
            active_jobs: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl ControlState {
    pub(super) fn is_draining(&self) -> bool {
        self.lease.as_ref().is_some_and(|lease| lease.drain)
    }

    pub(super) fn remaining(&mut self, now: Instant) -> Option<u64> {
        if self.lease.as_ref().is_some_and(|lease| lease.expires <= now) {
            self.lease = None;
        }
        self.lease.as_ref().map(|lease| {
            u64::try_from(lease.expires.saturating_duration_since(now).as_millis())
                .unwrap_or(u64::MAX)
                .max(1)
        })
    }
}

/// One admitted preparation, retained by its actual native cleanup owner.
/// It participates in the existing pool activity count without taking a worker slot.
#[derive(Debug)]
pub(crate) struct PreparationActivityGuard {
    active_jobs: Arc<AtomicUsize>,
}

impl Drop for PreparationActivityGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Actor access to the same admission boundary used by clients.
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
        if registry.control.remaining(Instant::now()).is_some() {
            return None;
        }
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
    fn try_begin_preparation_activity(&self, root: &Path) -> crate::error::Result<PreparationActivityGuard> {
        let mut inner = self.inner.lock();
        Self::check_runtime_admission(&mut inner, root)?;
        // The lease check and count change are one registry critical section.
        // Acquire cannot observe idle and win between admission and child spawn.
        let active_jobs = Arc::clone(&inner.control.active_jobs);
        active_jobs.fetch_add(1, Ordering::AcqRel);
        drop(inner); // Publish activity before another caller can acquire the lease.
        Ok(PreparationActivityGuard { active_jobs })
    }

    pub(crate) async fn preparation_activity(&self, root: &Path) -> crate::error::Result<PreparationActivityGuard> {
        let deadline = self.request_deadline()?;
        self.preparation_activity_until(root, deadline).await
    }

    async fn preparation_activity_until(
        &self,
        root: &Path,
        deadline: tokio::time::Instant,
    ) -> crate::error::Result<PreparationActivityGuard> {
        let cancellation = crate::cancellation::CallCancellation::current();
        loop {
            if cancellation.is_cancelled() {
                return Err(Self::pool_unavailable(root, "request_cancelled", false));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Self::pool_unavailable(root, "project_admission_timeout", true));
            }
            let result = self.try_begin_preparation_activity(root);
            if !matches!(&result, Err(crate::error::ServerError::WorkerUnavailable(info)) if info.reason == "worker_pool_draining")
            {
                return result;
            }
            // Drain waiters own no activity guard and cannot delay that drain.
            // An ordinary lease remains an immediate, explicitly retryable refusal.
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Self::pool_unavailable(root, "request_cancelled", false)),
                () = tokio::time::sleep_until(deadline) => return Err(Self::pool_unavailable(root, "project_admission_timeout", true)),
                () = tokio::time::sleep(Duration::from_millis(100)) => {},
            }
        }
    }

    /// This server's identity, as status, compatibility and file receipts name it.
    #[must_use]
    pub fn instance_id(&self) -> String {
        self.inner.lock().control.instance_id.clone()
    }
    /// Observe workers or atomically obtain, renew, or release an idle-pool lease.
    ///
    /// A successful acquire fences new worker admission before inspecting or
    /// retiring idle workers. Existing requests are never cancelled. Shutdown
    /// requests are not exit acknowledgements: inspect `workers_stopped` before
    /// relying on released memory, and renew the lease while waiting or working.
    #[must_use]
    pub fn runtime_control(&self, request: &RuntimeControlRequest) -> RuntimeControlResult {
        crate::worker_retirement::poll_retirements();
        let mut inner = self.inner.lock();
        let now = Instant::now();
        let deadline = now.checked_add(Duration::from_secs(
            request.lease_seconds.unwrap_or(DEFAULT_LEASE_SECONDS),
        ));
        inner.control.remaining(now);
        let mut result = RuntimeControlStatus::Ok;
        let mut detail = None;
        let mut token = None;
        let mut retiring = Vec::new();
        if let Some(error) = validate(request).or_else(|| deadline.is_none().then_some("lease deadline overflow")) {
            result = RuntimeControlStatus::InvalidRequest;
            detail = Some(error.to_owned());
        } else {
            match request.action {
                RuntimeAction::Status => {}
                RuntimeAction::Acquire | RuntimeAction::Drain => {
                    if inner.control.lease.is_some() {
                        result = RuntimeControlStatus::Leased;
                    } else if matches!(request.action, RuntimeAction::Acquire) && pool_busy(&inner) {
                        result = RuntimeControlStatus::Busy;
                    } else {
                        let lease_token = uuid::Uuid::new_v4().to_string();
                        inner.control.lease = Some(Lease {
                            token: lease_token.clone(),
                            expires: deadline.unwrap_or(now),
                            drain: matches!(request.action, RuntimeAction::Drain),
                            retire_idle: request.retire_idle,
                        });
                        token = Some(lease_token);
                        result = RuntimeControlStatus::Acquired;
                    }
                }
                RuntimeAction::Renew | RuntimeAction::Release => {
                    if let Some(lease) = inner
                        .control
                        .lease
                        .as_mut()
                        .filter(|lease| Some(&lease.token) == request.lease_id.as_ref())
                    {
                        if matches!(request.action, RuntimeAction::Renew) {
                            lease.expires = deadline.unwrap_or(now);
                        } else {
                            inner.control.lease = None;
                        }
                    } else {
                        result = RuntimeControlStatus::InvalidLease;
                    }
                }
            }
        }
        // Only the token owner can advance a drain. Status is observation only.
        if matches!(result, RuntimeControlStatus::Acquired | RuntimeControlStatus::Ok)
            && matches!(
                request.action,
                RuntimeAction::Acquire | RuntimeAction::Drain | RuntimeAction::Renew
            )
        {
            if pool_busy(&inner) {
                result = RuntimeControlStatus::Draining;
            } else if inner.control.lease.as_ref().is_some_and(|lease| lease.retire_idle) {
                let paths: Vec<_> = inner.registry.iter().map(|(root, _)| root.clone()).collect();
                for path in paths {
                    if let Some(project) = inner.registry.pop(&path) {
                        retiring.push(project);
                    }
                    inner.last_used.remove(&path);
                }
            }
        }
        let retirement_requested = retiring.len();
        // Closing a mailbox is non-blocking. Do it while admission is fenced;
        // the actor retains its semaphore permit through confirmed child exit.
        for project in &retiring {
            project.shutdown();
        }
        drop(retiring);
        let pool = self.pool_snapshot(&mut inner, Instant::now());
        if matches!(result, RuntimeControlStatus::Acquired | RuntimeControlStatus::Ok)
            && inner
                .control
                .lease
                .as_ref()
                .is_some_and(|lease| lease.drain && lease.retire_idle)
            && !pool.workers_stopped
            && matches!(request.action, RuntimeAction::Drain | RuntimeAction::Renew)
        {
            result = RuntimeControlStatus::Draining;
        }
        drop(inner);
        if matches!(
            result,
            RuntimeControlStatus::Acquired | RuntimeControlStatus::Draining | RuntimeControlStatus::Ok
        ) && matches!(
            request.action,
            RuntimeAction::Acquire | RuntimeAction::Drain | RuntimeAction::Renew
        ) && !pool.admission_paused
        {
            // A suspended thread can outlive even the minimum lease. Never
            // confirm a lease that this response already knows has expired.
            result = RuntimeControlStatus::InvalidLease;
            token = None;
        }
        RuntimeControlResult {
            build: crate::status::BuildIdentity::default(),
            effective_settings: crate::status::EffectiveSettings {
                broker: crate::status::BrokerSettings {
                    max_projects: self.config.max_projects.get(),
                    max_workers: self.config.max_projects.get(),
                    max_workers_per_project: self.config.max_workers_per_project.get(),
                    idle_timeout_secs: self.config.idle_timeout.as_secs(),
                },
                runtime: crate::status::RuntimeSettings::from(&self.runtime_config),
                server: None,
            },
            capabilities: vec!["draining_lease_v1".to_owned()],
            requests: None,
            status: result,
            detail,
            lease_id: token,
            retirement_requested,
            pool,
        }
    }

    fn pool_snapshot(&self, inner: &mut BrokerInner, now: Instant) -> WorkerPoolStatus {
        let remaining = inner.control.remaining(now);
        let occupied_slots = self
            .config
            .max_projects
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
        WorkerPoolStatus {
            instance_id: inner.control.instance_id.clone(),
            max_projects: self.config.max_projects.get(),
            max_workers: self.config.max_projects.get(),
            max_workers_per_project: self.config.max_workers_per_project.get(),
            occupied_slots,
            outstanding_jobs: inner.control.active_jobs.load(Ordering::Acquire),
            workers_stopped: occupied_slots == 0
                && acquiring_projects.is_empty()
                && inner.control.active_jobs.load(Ordering::Acquire) == 0,
            admission_paused: remaining.is_some(),
            lease_remaining_millis: remaining,
            acquiring_projects,
            projects,
        }
    }
}

fn pool_busy(inner: &BrokerInner) -> bool {
    inner.control.active_jobs.load(Ordering::Acquire) > 0
        || inner.opening_locks.values().any(|lock| lock.strong_count() > 0)
        || inner
            .registry
            .iter()
            .any(|(_, project)| !project.is_idle() || Arc::strong_count(project) != 1)
}

fn validate(request: &RuntimeControlRequest) -> Option<&'static str> {
    match request.action {
        RuntimeAction::Status => {
            if request.lease_id.is_some() || request.lease_seconds.is_some() || request.retire_idle {
                return Some("status takes no lease options");
            }
        }
        RuntimeAction::Acquire | RuntimeAction::Drain => {
            if request.lease_id.is_some() {
                return Some("acquire does not take an existing lease_id");
            }
        }
        RuntimeAction::Renew | RuntimeAction::Release => {
            if request.lease_id.as_ref().is_none_or(|token| token.is_empty()) {
                return Some("renew and release require lease_id");
            }
            if request.retire_idle {
                return Some("retire_idle is only valid for acquire");
            }
            if matches!(request.action, RuntimeAction::Release) && request.lease_seconds.is_some() {
                return Some("release does not take lease_seconds");
            }
        }
    }
    if request
        .lease_seconds
        .is_some_and(|seconds| seconds == 0 || seconds > MAX_LEASE_SECONDS)
    {
        return Some("lease_seconds must be between 1 and 300");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn broker() -> Arc<ProjectBroker> {
        ProjectBroker::new(super::super::BrokerConfig {
            env_default: None,
            cwd: PathBuf::from("/unused"),
            max_projects: std::num::NonZeroUsize::MIN,
            max_workers_per_project: std::num::NonZeroUsize::MIN,
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
        let status = broker.runtime_control(&RuntimeControlRequest::default());
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

    fn acquire() -> RuntimeControlRequest {
        RuntimeControlRequest {
            action: RuntimeAction::Acquire,
            ..RuntimeControlRequest::default()
        }
    }

    fn token_request(action: RuntimeAction, token: &str) -> RuntimeControlRequest {
        RuntimeControlRequest {
            action,
            lease_id: Some(token.to_owned()),
            ..RuntimeControlRequest::default()
        }
    }

    #[test]
    fn lease_token_fences_admission_and_cannot_be_obtained_from_status() -> TestResult {
        let broker = broker();
        let claimed = broker.runtime_control(&acquire());
        assert_eq!(claimed.status, RuntimeControlStatus::Acquired);
        let token = claimed.lease_id.ok_or("missing acquired token")?;
        assert!(claimed.pool.admission_paused && claimed.pool.workers_stopped);
        let observed = broker.runtime_control(&RuntimeControlRequest::default());
        assert!(observed.lease_id.is_none());
        assert_eq!(observed.pool.instance_id, claimed.pool.instance_id);
        assert!(broker.opening_lock(Path::new("/blocked")).is_err());
        assert_eq!(broker.runtime_control(&acquire()).status, RuntimeControlStatus::Leased);
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, "wrong"))
                .status,
            RuntimeControlStatus::InvalidLease
        );
        assert!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .admission_paused
        );
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Renew, &token))
                .status,
            RuntimeControlStatus::Ok
        );
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, &token))
                .status,
            RuntimeControlStatus::Ok
        );
        assert!(broker.opening_lock(Path::new("/unblocked")).is_ok());
        Ok(())
    }

    #[test]
    fn expiry_reopens_admission_and_old_token_cannot_release_a_successor() -> TestResult {
        let broker = broker();
        let first = broker
            .runtime_control(&acquire())
            .lease_id
            .ok_or("missing first token")?;
        {
            let mut inner = broker.inner.lock();
            let lease = inner.control.lease.as_mut().ok_or("missing first lease")?;
            lease.expires = Instant::now();
            drop(inner);
        }
        let opening = broker.opening_lock(Path::new("/after-expiry"))?;
        assert_eq!(broker.runtime_control(&acquire()).status, RuntimeControlStatus::Busy);
        drop(opening);
        let second = broker
            .runtime_control(&acquire())
            .lease_id
            .ok_or("missing successor token")?;
        assert_ne!(first, second);
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, &first))
                .status,
            RuntimeControlStatus::InvalidLease
        );
        assert!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .admission_paused
        );
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, &second))
                .status,
            RuntimeControlStatus::Ok
        );
        Ok(())
    }

    #[test]
    fn claiming_and_startup_admission_are_mutually_exclusive() -> TestResult {
        // Retain each successful opening Arc until both racing decisions have
        // completed; otherwise completing the request would legitimately allow
        // both operations to succeed in sequence.
        for _ in 0..100 {
            let broker = broker();
            let ready = Arc::new(Barrier::new(2));
            let finish = Arc::new(Barrier::new(2));
            let opener = Arc::clone(&broker);
            let opener_ready = Arc::clone(&ready);
            let opener_finish = Arc::clone(&finish);
            let handle = std::thread::spawn(move || {
                opener_ready.wait();
                let admitted = opener.opening_lock(Path::new("/racing"));
                opener_finish.wait();
                admitted.is_ok()
            });
            ready.wait();
            let claim = broker.runtime_control(&acquire());
            finish.wait();
            let admitted = handle.join().map_err(|_| "startup race thread panicked")?;
            assert_ne!(admitted, claim.status == RuntimeControlStatus::Acquired);
        }
        Ok(())
    }

    #[test]
    fn invalid_options_never_create_or_release_a_lease() -> TestResult {
        let broker = broker();
        for seconds in [0, MAX_LEASE_SECONDS + 1] {
            let invalid = RuntimeControlRequest {
                lease_seconds: Some(seconds),
                ..acquire()
            };
            assert_eq!(
                broker.runtime_control(&invalid).status,
                RuntimeControlStatus::InvalidRequest
            );
            assert!(
                !broker
                    .runtime_control(&RuntimeControlRequest::default())
                    .pool
                    .admission_paused
            );
        }
        let claimed = broker.runtime_control(&acquire());
        let token = claimed.lease_id.ok_or("missing token")?;
        let wrong_instance = self::broker();
        assert_eq!(
            wrong_instance
                .runtime_control(&token_request(RuntimeAction::Renew, &token))
                .status,
            RuntimeControlStatus::InvalidLease
        );
        Ok(())
    }

    #[tokio::test]
    async fn retirement_acknowledgement_is_not_reported_as_confirmed_exit() -> TestResult {
        let broker = broker();
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or("deadline out of range")?;
        let retiring = broker
            .reserve_worker_slot(Path::new("/uncertain-retirement"), deadline)
            .await?;
        let claimed = broker.runtime_control(&RuntimeControlRequest {
            retire_idle: true,
            ..acquire()
        });
        assert_eq!(claimed.status, RuntimeControlStatus::Acquired);
        assert_eq!(claimed.pool.occupied_slots, 1);
        assert!(!claimed.pool.workers_stopped);
        assert!(claimed.pool.admission_paused);
        drop(retiring);
        let stopped = broker.runtime_control(&RuntimeControlRequest::default());
        assert!(stopped.pool.workers_stopped && stopped.pool.admission_paused);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and matching worker"]
    async fn background_maintenance_is_fenced_and_detached_activity_stays_counted() -> TestResult {
        let broker = broker();
        let root = PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE")?);
        let call = broker
            .admitted_project_runtime(super::super::ProjectHint::Explicit(root), vec![])
            .await?;
        let root = Path::new(&call.freshness.project_root);
        let access = MaintenanceAccess::new(&broker.inner);
        let activity = access
            .try_begin(root, &call.freshness.session_id)
            .ok_or("maintenance was not admitted")?;
        assert_eq!(broker.runtime_control(&acquire()).status, RuntimeControlStatus::Busy);
        assert_eq!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .outstanding_jobs,
            1
        );
        drop(activity);
        let claimed = broker.runtime_control(&acquire());
        let token = claimed.lease_id.ok_or("missing lease token")?;
        assert!(access.try_begin(root, &call.freshness.session_id).is_none());
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, &token))
                .status,
            RuntimeControlStatus::Ok
        );
        let detached = access
            .try_begin(root, &call.freshness.session_id)
            .ok_or("maintenance did not resume")?;
        broker.shutdown_all();
        let busy = broker.runtime_control(&acquire());
        assert_eq!(busy.status, RuntimeControlStatus::Busy);
        assert!(busy.pool.projects.is_empty());
        assert!(!busy.pool.workers_stopped);
        drop(detached);
        let next = broker.runtime_control(&acquire());
        assert_eq!(next.status, RuntimeControlStatus::Acquired);
        let token = next.lease_id.ok_or("missing successor token")?;
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Release, &token))
                .status,
            RuntimeControlStatus::Ok
        );
        assert!(access.try_begin(root, &call.freshness.session_id).is_none());
        Ok(())
    }
    fn drain() -> RuntimeControlRequest {
        RuntimeControlRequest {
            action: RuntimeAction::Drain,
            ..RuntimeControlRequest::default()
        }
    }

    #[test]
    fn drain_fences_new_callers_until_existing_opening_finishes() -> TestResult {
        let broker = broker();
        let opening = broker.opening_lock(Path::new("/existing"))?;
        let result = broker.runtime_control(&drain());
        assert_eq!(result.status, RuntimeControlStatus::Draining);
        let token = result.lease_id.ok_or("missing drain token")?;
        assert!(broker.opening_lock(Path::new("/later")).is_err());
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Renew, &token))
                .status,
            RuntimeControlStatus::Draining
        );
        drop(opening);
        assert_eq!(
            broker
                .runtime_control(&token_request(RuntimeAction::Renew, &token))
                .status,
            RuntimeControlStatus::Ok
        );
        assert!(broker.opening_lock(Path::new("/later")).is_err());
        drop(broker.runtime_control(&token_request(RuntimeAction::Release, &token)));
        assert!(broker.opening_lock(Path::new("/later")).is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn already_admitted_opener_can_get_capacity_during_drain() -> TestResult {
        let broker = broker();
        let opening = broker.opening_lock(Path::new("/existing"))?;
        let result = broker.runtime_control(&drain());
        assert_eq!(result.status, RuntimeControlStatus::Draining);
        let slot = broker
            .reserve_worker_slot(
                Path::new("/existing"),
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await?;
        drop(slot);
        drop(opening);
        Ok(())
    }

    #[tokio::test]
    async fn later_caller_waits_without_counting_as_admitted() -> TestResult {
        let broker = broker();
        let result = broker.runtime_control(&drain());
        let token = result.lease_id.ok_or("missing drain token")?;
        let waiter_broker = Arc::clone(&broker);
        let waiter = tokio::spawn(async move {
            waiter_broker
                .acquire_until(
                    PathBuf::from("/does-not-exist"),
                    tokio::time::Instant::now() + Duration::from_millis(200),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let state = broker.runtime_control(&token_request(RuntimeAction::Renew, &token));
        assert_eq!(state.status, RuntimeControlStatus::Ok);
        assert!(state.pool.acquiring_projects.is_empty());
        assert_eq!(state.pool.outstanding_jobs, 0);
        let result = waiter.await?;
        assert!(matches!(result, Err(crate::error::ServerError::WorkerUnavailable(info))
            if info.reason == "project_admission_timeout"));
        Ok(())
    }

    #[tokio::test]
    async fn drain_expiry_wakes_waiter_without_an_api_call() -> TestResult {
        let broker = broker();
        let result = broker.runtime_control(&drain());
        assert!(result.lease_id.is_some());
        {
            let mut inner = broker.inner.lock();
            inner.control.lease.as_mut().ok_or("missing lease")?.expires = Instant::now() + Duration::from_millis(40);
        }
        let result = broker
            .acquire_until(
                PathBuf::from("/does-not-exist"),
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await;
        assert!(
            !matches!(result, Err(crate::error::ServerError::WorkerUnavailable(info))
            if info.reason == "project_admission_timeout" || info.reason == "worker_pool_draining")
        );
        assert!(
            !broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .admission_paused
        );
        Ok(())
    }
    #[tokio::test]
    async fn detached_startup_holds_drain_until_its_opening_guard_drops() -> TestResult {
        let broker = broker();
        let opening = broker.opening_lock(Path::new("/detached"))?;
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
        let result = broker.runtime_control(&drain());
        let token = result.lease_id.ok_or("missing drain token")?;
        assert_eq!(result.status, RuntimeControlStatus::Draining);
        assert!(!result.pool.acquiring_projects.is_empty());
        finish_tx.send(())?;
        startup.await?;
        let result = broker.runtime_control(&token_request(RuntimeAction::Renew, &token));
        assert_eq!(result.status, RuntimeControlStatus::Ok);
        assert!(result.pool.workers_stopped);
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_native_drain_waiter_releases_no_other_owners_lease() -> TestResult {
        let broker = broker();
        let result = broker.runtime_control(&drain());
        let token = result.lease_id.ok_or("missing lease")?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let waiting_cancel = cancel.clone();
        let waiting_broker = Arc::clone(&broker);
        let waiter = tokio::spawn(async move {
            crate::cancellation::with_cancellation(
                &waiting_cancel,
                waiting_broker.acquire_until(
                    PathBuf::from("/no-worker-needed"),
                    tokio::time::Instant::now() + Duration::from_secs(30),
                ),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), waiter).await??;
        assert!(matches!(result, Err(crate::error::ServerError::WorkerUnavailable(info))
            if info.reason == "request_cancelled"));
        let state = broker.runtime_control(&token_request(RuntimeAction::Renew, &token));
        assert_eq!(state.status, RuntimeControlStatus::Ok);
        assert!(state.pool.admission_paused);
        assert!(state.pool.acquiring_projects.is_empty());
        assert_eq!(state.pool.outstanding_jobs, 0);
        Ok(())
    }

    #[test]
    fn preparation_activity_blocks_idle_acquire_without_a_worker_slot() -> TestResult {
        let broker = broker();
        let activity = broker.try_begin_preparation_activity(Path::new("/preparation"))?;
        let busy = broker.runtime_control(&acquire());
        assert_eq!(busy.status, RuntimeControlStatus::Busy);
        assert_eq!(busy.pool.occupied_slots, 0);
        assert_eq!(busy.pool.outstanding_jobs, 1);
        assert!(!busy.pool.workers_stopped);
        drop(activity);
        let acquired = broker.runtime_control(&acquire());
        assert_eq!(acquired.status, RuntimeControlStatus::Acquired);
        assert_eq!(acquired.pool.outstanding_jobs, 0);
        assert!(acquired.pool.workers_stopped);
        Ok(())
    }

    #[test]
    fn preparation_and_idle_lease_share_the_atomic_admission_boundary() -> TestResult {
        let broker = broker();
        let barrier = Arc::new(Barrier::new(2));
        let activity_broker = Arc::clone(&broker);
        let activity_barrier = Arc::clone(&barrier);
        let activity = std::thread::spawn(move || {
            activity_barrier.wait();
            activity_broker.try_begin_preparation_activity(Path::new("/racing-preparation"))
        });
        barrier.wait();
        let claimed = broker.runtime_control(&acquire());
        let activity = activity.join().map_err(|_| "preparation admission panicked")?;
        match (claimed.status, activity) {
            (RuntimeControlStatus::Busy, Ok(guard)) => drop(guard),
            (RuntimeControlStatus::Acquired, Err(crate::error::ServerError::WorkerUnavailable(info)))
                if info.reason == "worker_pool_leased" => {}
            (status, result) => return Err(format!("non-atomic preparation admission: {status:?} {result:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn preparation_drain_waits_for_owned_activity_then_resumes_on_release() -> TestResult {
        let broker = broker();
        let activity = broker.try_begin_preparation_activity(Path::new("/admitted-preparation"))?;
        let draining = broker.runtime_control(&drain());
        let token = draining.lease_id.ok_or("missing drain token")?;
        assert_eq!(draining.status, RuntimeControlStatus::Draining);
        let waiter = broker.preparation_activity(Path::new("/waiting-preparation"));
        tokio::pin!(waiter);
        tokio::select! {
            result = &mut waiter => return Err(format!("preparation overtook drain: {result:?}").into()),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert_eq!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .outstanding_jobs,
            1
        );
        drop(activity);
        let renewed = broker.runtime_control(&token_request(RuntimeAction::Renew, &token));
        assert_eq!(renewed.status, RuntimeControlStatus::Ok);
        assert!(renewed.pool.workers_stopped);
        let released = broker.runtime_control(&token_request(RuntimeAction::Release, &token));
        assert_eq!(released.status, RuntimeControlStatus::Ok);
        let activity = tokio::time::timeout(Duration::from_secs(1), waiter).await??;
        assert_eq!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .outstanding_jobs,
            1
        );
        drop(activity);
        Ok(())
    }

    #[tokio::test]
    async fn preparation_drain_waiter_keeps_its_original_deadline_without_activity() -> TestResult {
        let broker = broker();
        let draining = broker.runtime_control(&drain());
        let token = draining.lease_id.ok_or("missing drain token")?;
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(30))
            .ok_or("deadline overflow")?;
        let result = broker
            .preparation_activity_until(Path::new("/waiting-preparation"), deadline)
            .await;
        assert!(
            matches!(result, Err(crate::error::ServerError::WorkerUnavailable(info)) if info.reason == "project_admission_timeout")
        );
        let renewed = broker.runtime_control(&token_request(RuntimeAction::Renew, &token));
        assert_eq!(renewed.status, RuntimeControlStatus::Ok);
        assert_eq!(renewed.pool.outstanding_jobs, 0);
        assert!(renewed.pool.workers_stopped);
        Ok(())
    }

    #[tokio::test]
    async fn expired_preparation_deadline_never_admits_even_when_unleased() -> TestResult {
        let broker = broker();
        let expired = tokio::time::Instant::now()
            .checked_sub(Duration::from_millis(1))
            .ok_or("deadline underflow")?;
        let result = broker
            .preparation_activity_until(Path::new("/expired-preparation"), expired)
            .await;
        assert!(
            matches!(result, Err(crate::error::ServerError::WorkerUnavailable(info)) if info.reason == "project_admission_timeout")
        );
        let status = broker.runtime_control(&RuntimeControlRequest::default());
        assert_eq!(status.pool.outstanding_jobs, 0);
        assert!(status.pool.workers_stopped);
        Ok(())
    }
}
