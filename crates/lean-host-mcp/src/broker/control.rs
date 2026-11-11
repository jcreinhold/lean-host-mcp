//! Atomic worker-pool observation and expiring admission leases.
//!
//! The registry lock is the admission boundary: a caller holds a project Arc
//! before using a resident worker, or an opening-lock Arc before starting one.
//! A lease can therefore be granted only before admission or after all such
//! callers and actor-owned messages have finished. Network connections are not
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
    /// Filled by the service that owns the retained-request registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_registry: Option<crate::status::RequestRegistryStatus>,
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
    /// Slots still owned, including shutdowns whose exit is not confirmed.
    pub occupied_slots: usize,
    /// Actor work across all controllers, including those removed from the registry.
    pub outstanding_jobs: usize,
    /// True only when this server has confirmed release of every worker slot.
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

    pub(crate) fn activity_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active_jobs)
    }

    pub(crate) fn try_begin(&self, root: &Path, session_id: &str) -> Option<crate::project::ActiveJobGuard> {
        let inner = self.inner.upgrade()?;
        let mut registry = inner.lock();
        if registry.control.remaining(Instant::now()).is_some() {
            return None;
        }
        // An evicted/replaced controller cannot start another idle recycle.
        let guard = registry.registry.peek(root)?.begin_maintenance(session_id);
        drop(registry);
        guard
    }
}

impl ProjectBroker {
    /// Identity shared by status and immutable retained-request bindings.
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
                RuntimeAction::Acquire => {
                    if inner.control.lease.is_some() {
                        result = RuntimeControlStatus::Leased;
                    } else if pool_busy(&inner) {
                        result = RuntimeControlStatus::Busy;
                    } else {
                        let lease_token = uuid::Uuid::new_v4().to_string();
                        inner.control.lease = Some(Lease {
                            token: lease_token.clone(),
                            expires: deadline.unwrap_or(now),
                        });
                        token = Some(lease_token);
                        result = RuntimeControlStatus::Acquired;
                        if request.retire_idle {
                            let paths: Vec<_> = inner.registry.iter().map(|(root, _)| root.clone()).collect();
                            for path in paths {
                                if let Some(project) = inner.registry.pop(&path) {
                                    retiring.push(project);
                                }
                                inner.last_used.remove(&path);
                            }
                        }
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
        let retirement_requested = retiring.len();
        // Closing a mailbox is non-blocking. Do it while admission is fenced;
        // the actor retains its semaphore permit through confirmed child exit.
        for project in &retiring {
            project.shutdown();
        }
        drop(retiring);
        let pool = self.pool_snapshot(&mut inner, Instant::now());
        drop(inner);
        if matches!(result, RuntimeControlStatus::Acquired | RuntimeControlStatus::Ok)
            && matches!(request.action, RuntimeAction::Acquire | RuntimeAction::Renew)
            && !pool.admission_paused
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
                    idle_timeout_secs: self.config.idle_timeout.as_secs(),
                },
                runtime: crate::status::RuntimeSettings::from(&self.runtime_config),
                server: None,
            },
            capabilities: vec!["retained_request_binding_v1".to_owned()],
            request_registry: None,
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
                    project_root: root.to_string_lossy().into_owned(),
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
        RuntimeAction::Acquire => {
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
            config_default: None,
            env_default: None,
            cwd: PathBuf::from("/unused"),
            max_projects: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        })
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
}
