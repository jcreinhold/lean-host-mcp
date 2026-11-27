//! `LeanProject`—the unit of Lean semantic execution.
//!
//! One Lake project owns one private serialized controller. The controller
//! submits one worker request at a time, applies host memory/retry policy, and
//! exposes only typed request/reply calls to tool modules. The lower
//! `lean-rs-worker-parent` service owns child-process shutdown, generation
//! separation, terminal outcomes, and primitive restart mechanics.

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerCancellationToken, LeanWorkerChild, LeanWorkerDeclarationInspectionRequest,
    LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationSearch, LeanWorkerDeclarationSearchResult,
    LeanWorkerDeclarationVerificationBatchRequest, LeanWorkerDeclarationVerificationBatchResult,
    LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions,
    LeanWorkerError, LeanWorkerHostHandle, LeanWorkerHostHandleBuilder, LeanWorkerLifecycleSnapshot,
    LeanWorkerModuleCacheLimits, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets, LeanWorkerProcessTracker,
    LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult, LeanWorkerRequestStopReceipt, LeanWorkerRestartPolicy,
};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::cancellation::{CallCancellation, CallOperation};
use crate::envelope::{FinalAttemptStatus, Freshness, RuntimeFacts, RuntimeRestartEvent, WorkerStopReceipt};
use crate::error::{Result, ServerError, WorkerUnavailable, map_worker_err};
use crate::lake_meta::LakeProjectMeta;
use crate::toolchain::{Readiness, ToolchainId, WorkerBinary};
use crate::worker_retirement::WorkerSlot;

mod restart;
mod runtime_config;
mod semantic_runtime;

use restart::{
    RestartCause, RestartStats, log_restart, millis_u64, restart_cause_from_worker, restart_event, restart_reason_text,
    worker_death_cause,
};
pub use runtime_config::ProjectRuntimeConfig;
use runtime_config::RESTART_WINDOW;
pub(crate) use semantic_runtime::{build_semantic_runtime, semantic_runtime};

/// LRU capacity for exact bounded module-query results, applied to the single
/// and batch caches separately.
///
/// This bounds memory outright rather than approximately. Every cached value is
/// a worker reply already truncated to the 64 KiB `total_bytes` budget the tools
/// send (`DEFAULT_TOTAL_BYTES` in `tools/`), so one project holds at most
/// `2 × 64 × 64 KiB = 8 MiB` and the default four-project pool at most 32 MiB —
/// no byte-accounting LRU required, which is the machinery this design avoids.
///
/// 64 rather than the former 256 because the working set is small and concrete:
/// an agent iterates on a handful of files, and each file contributes one entry
/// per distinct query shape. 256 sized the cache for a workload nobody has;
/// what it actually bought was a 4× larger worst case.
const MODULE_QUERY_CACHE_CAPACITY: usize = 64;
/// Pure backstop on imports per child, behind the byte budget.
///
/// The one thing between a residue-accounting bug and the 11.2 GiB `SIGKILL`
/// this subsystem exists to prevent, and it costs one integer compare. Set far
/// above any legitimate generation so that in normal operation the byte bound is
/// always what fires and this only speaks when residue is being reported as
/// zero. [`lean_rs_worker_parent::LeanWorkerStats::import_stats_unusable`]
/// says which happened.
const WORKER_MAX_IMPORTS_BACKSTOP: u64 = 32;
/// How long the actor waits for another call before treating the project as
/// quiescent and cycling an over-budget child.
///
/// Applied only when the child is above its soft residue budget. The
/// independent lifecycle poll may observe quiescence up to one tick later.
const QUIESCENCE_GRACE: Duration = Duration::from_secs(2);
/// Observe owned-child death and cleanup when no new job arrives.
const WORKER_LIVENESS_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How recently a profile must have been served for an idle cycle to re-import
/// it into the fresh child.
///
/// A minute of silence is not evidence the caller is coming back to the same
/// file, and a wrong guess costs a full import.
const PREWARM_RECENCY: Duration = Duration::from_mins(1);
/// Backstop on *every* worker cycle in one [`RESTART_WINDOW`], planned or not.
///
/// The supervisor's own limit, distinct from
/// [`ProjectRuntimeConfig::max_restarts_per_window`], which this actor applies
/// to abnormal causes only. The supervisor cannot make that distinction, so its
/// default of 16 counts planned cycles too —
/// and exhausting it is terminal: the supervisor refuses to spawn a
/// replacement and every later call fails with "shutdown is in progress". A
/// workload that alternates import profiles reached that state after 16 calls
/// and never recovered. Sized far above any legitimate cycle rate so that only
/// a genuine spawn loop can reach it; the discriminating breaker is this
/// actor's own.
const SUPERVISOR_RESTART_INTENSITY: u64 = 256;

/// Result of one project actor call.
#[derive(Debug, Clone)]
pub(crate) struct ProjectCall<T> {
    value: T,
    runtime: RuntimeFacts,
}

impl<T> ProjectCall<T> {
    pub(crate) fn new(value: T, runtime: RuntimeFacts) -> Self {
        Self { value, runtime }
    }

    pub(crate) fn into_parts(self) -> (T, RuntimeFacts) {
        (self.value, self.runtime)
    }
}

/// Lean elaboration and commands may perform IO. Recovery never replays a
/// submitted job, even when its direct child is known to have stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryPolicy {
    Never,
}

impl RetryPolicy {
    fn retries(self) -> u32 {
        match self {
            Self::Never => 0,
        }
    }
}

pub(crate) struct ActiveJobGuard {
    active_jobs: Arc<AtomicUsize>,
    pool_active_jobs: Arc<AtomicUsize>,
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::AcqRel);
        self.pool_active_jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How a `run_job` retry loop ended. Carrying this out of the loop keeps the
/// loop's arms to control flow only — the caller computes timing and restart
/// facts at one finalization point instead of at every exit.
enum JobExit<R> {
    /// The job produced a value.
    Done(R),
    /// The worker could not produce a value; the fields are the arguments
    /// [`ProjectActorState::worker_unavailable_for`] needs.
    Unavailable {
        reason: String,
        retryable: bool,
        worker_restarted: bool,
        cause: Option<RestartCause>,
        limit_kib: Option<u64>,
    },
}

/// A worker-child CPU sample pinned to the child generation it came from, so a
/// mid-call recycle (new child, counter reset to zero) yields `None` from
/// [`ProjectActorState::cpu_delta_since`] instead of a bogus delta.
struct CpuSampleBaseline {
    worker_generation: u64,
    cpu_millis: u64,
}

/// One atomic decision separates queue expiry from dispatch. Once expired, a
/// message can stay in the mailbox but can never reach the worker. The broker
/// supplies its original admission deadline; entering this queue adds no budget.
struct QueueDeadline {
    deadline: Instant,
    state: AtomicUsize,
}

impl QueueDeadline {
    const WAITING: usize = 0;
    const STARTED: usize = 1;
    const EXPIRED: usize = 2;

    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            state: AtomicUsize::new(Self::WAITING),
        }
    }

    fn expire(&self) -> bool {
        self.state
            .compare_exchange(Self::WAITING, Self::EXPIRED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            || self.state.load(Ordering::Acquire) == Self::EXPIRED
    }

    fn start(&self) -> bool {
        if Instant::now() >= self.deadline {
            self.expire();
            return false;
        }
        self.state
            .compare_exchange(Self::WAITING, Self::STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

struct JobMeta {
    file_label: Option<String>,
    queue: Arc<QueueDeadline>,
    imports: Vec<String>,
    import_fingerprint: String,
    _created_at: Instant,
    queued_at: Instant,
    correlation_id: uuid::Uuid,
    retry_policy: RetryPolicy,
    _active_job: ActiveJobGuard,
    cancellation: CallCancellation,
    /// The finished-build count of the request's read turn, or `None` when the
    /// job was sent without one.
    granted_at: Option<u64>,
}

enum ProjectMessage {
    NativeCompatibility {
        meta: JobMeta,
        reply: oneshot::Sender<Result<ProjectCall<()>>>,
    },
    ModuleQuery {
        meta: JobMeta,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerModuleQueryOutcome>>>,
    },
    ModuleQueryBatch {
        meta: JobMeta,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>>>,
    },
    DeclarationInspection {
        meta: JobMeta,
        request: LeanWorkerDeclarationInspectionRequest,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationInspectionResult>>>,
    },
    /// Several searches against **one** session; see
    /// [`LeanProject::search_declarations`].
    DeclarationSearch {
        meta: JobMeta,
        requests: Vec<LeanWorkerDeclarationSearch>,
        reply: oneshot::Sender<Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>>>,
    },
    ProofAttempt {
        meta: JobMeta,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerProofAttemptResult>>>,
    },
    DeclarationVerification {
        meta: JobMeta,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationVerificationResult>>>,
    },
    DeclarationVerificationBatch {
        meta: JobMeta,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>>>,
    },
}

impl ProjectMessage {
    fn meta(&self) -> &JobMeta {
        match self {
            Self::NativeCompatibility { meta, .. }
            | Self::ModuleQuery { meta, .. }
            | Self::ModuleQueryBatch { meta, .. }
            | Self::DeclarationInspection { meta, .. }
            | Self::DeclarationSearch { meta, .. }
            | Self::ProofAttempt { meta, .. }
            | Self::DeclarationVerification { meta, .. }
            | Self::DeclarationVerificationBatch { meta, .. } => meta,
        }
    }

    fn imports(&self) -> &[String] {
        &self.meta().imports
    }

    fn reject(self, state: &ProjectActorState, reason: &'static str) {
        match self {
            Self::NativeCompatibility { meta, reply } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::ModuleQuery { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::ModuleQueryBatch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationInspection { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationSearch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::ProofAttempt { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationVerification { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
            Self::DeclarationVerificationBatch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    worker_generation: u64,
    last_restart: Option<RuntimeRestartEvent>,
    rss_kib: Option<u64>,
    import_profile: Option<String>,
    profile_switch_count: u64,
    restarts_total: u64,
    restarts_by_cause: BTreeMap<String, u64>,
    import_residue_bytes: Option<u64>,
    import_residue_limit_bytes: Option<u64>,
}

impl RuntimeSnapshot {
    fn facts(&self) -> RuntimeFacts {
        RuntimeFacts {
            native_compatibility: None,
            cli_file_verification: None,
            worker_generation: self.worker_generation,
            worker_restarted: false,
            retry_count: 0,
            final_attempt: FinalAttemptStatus::Unknown,
            previous_attempt_failure: None,
            queue_wait_millis: 0,
            call_elapsed_millis: 0,
            call_cpu_millis: None,
            call_restart: None,
            last_restart: self.last_restart.clone(),
            rss_kib: self.rss_kib,
            worker_lanes: 1,
            import_profile: self.import_profile.clone(),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restarts_total,
            restarts_by_cause: self.restarts_by_cause.clone(),
            import_residue_bytes: self.import_residue_bytes,
            import_residue_limit_bytes: self.import_residue_limit_bytes,
        }
    }
}

/// One Lake project, one serialized worker controller, one in-memory cache.
/// Cheap to clone via `Arc`.
pub(crate) struct LeanProject {
    canonical_root: PathBuf,
    toolchain: String,
    manifest_hash: String,
    session_id: String,
    /// Toolchain-provenance advisories captured at open (unknown pin, missing
    /// sidecar). Surfaced into every response's envelope warnings via
    /// [`Self::freshness`]; empty for a fully-vouched-for worker.
    open_warnings: Vec<String>,
    actor_tx: Mutex<Option<mpsc::Sender<ProjectMessage>>>,
    active_jobs: Arc<AtomicUsize>,
    pool_active_jobs: Arc<AtomicUsize>,
    healthy: Arc<AtomicBool>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    module_queries: ModuleQueryCache,
    /// Alive while the actor thread can still start a worker.
    actor_scope: std::sync::Weak<()>,
    /// Counts this actor's worker processes until each exit is confirmed.
    process_tracker: LeanWorkerProcessTracker,
}

/// Reserve a bounded mailbox slot without losing FIFO position on a wakeup.
async fn reserve_mailbox<'a, T>(
    tx: &'a mpsc::Sender<T>,
    cancellation: &CallCancellation,
    limit: Duration,
) -> std::result::Result<mpsc::Permit<'a, T>, &'static str> {
    if cancellation.is_cancelled() {
        return Err("request_cancelled");
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err("request_cancelled"),
        result = tokio::time::timeout(limit, tx.reserve()) => match result {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err("mailbox_closed"),
            Err(_) => Err("mailbox_admission_timeout"),
        },
    }
}

impl std::fmt::Debug for LeanProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeanProject")
            .field("canonical_root", &self.canonical_root)
            .field("toolchain", &self.toolchain)
            .field("manifest_hash", &self.manifest_hash)
            .finish_non_exhaustive()
    }
}

impl LeanProject {
    pub(crate) fn open(
        meta: LakeProjectMeta,
        runtime_config: ProjectRuntimeConfig,
        slot: OwnedSemaphorePermit,
        maintenance: crate::broker::MaintenanceAccess,
        prepared_host_artifacts: lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts,
        initialization: CallOperation,
    ) -> Result<Arc<Self>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
            import_residue_bytes: None,
            import_residue_limit_bytes: None,
        }));
        let active_jobs = Arc::new(AtomicUsize::new(0));
        let pool_active_jobs = maintenance.activity_counter();
        let healthy = Arc::new(AtomicBool::new(true));
        let (mut config, open_warnings) = match ActorConfig::from_meta(
            &meta,
            session_id.clone(),
            Arc::clone(&runtime),
            Arc::clone(&healthy),
            runtime_config,
        ) {
            Ok(value) => value,
            Err(error) => {
                initialization.not_started();
                return Err(error);
            }
        };
        config.maintenance = maintenance;
        config.prepared_host_artifacts = Some(prepared_host_artifacts);
        type InitMsg = std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let thread_name = actor_thread_name(&meta.canonical_root);

        // Retain the not-yet-dispatched guard until thread creation succeeds;
        // std::thread drops a failed spawn's closure before returning its error.
        let handoff = Arc::new(Mutex::new(Some(initialization)));
        let actor_handoff = Arc::clone(&handoff);
        let process_tracker = config.process_tracker.clone();
        let scope = Arc::new(());
        let actor_scope = Arc::downgrade(&scope);
        if let Err(error) = thread::Builder::new().name(thread_name).spawn(move || {
            let Some(initialization) = actor_handoff.lock().take() else {
                return;
            };
            // The permit outlives shutdown, failed startup and unwinding.
            let slot = WorkerSlot::new(slot, config.process_tracker.clone());
            actor_main(config, init_tx, initialization);
            // The captured scope outlives the slot, even when unwinding. Once
            // it is gone, no code of this actor can start another worker.
            drop(slot);
            drop(scope);
        }) {
            let initialization = handoff.lock().take();
            if let Some(initialization) = initialization {
                initialization.not_started();
            }
            return Err(ServerError::Internal(format!("spawn project actor thread: {error}")));
        }
        drop(handoff);

        let (runtime_toolchain, actor_tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("project actor thread died during init".into()))??;

        let cache_cap = NonZeroUsize::new(MODULE_QUERY_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        Ok(Arc::new(Self {
            canonical_root: meta.canonical_root,
            toolchain: runtime_toolchain,
            manifest_hash: meta.manifest_hash,
            session_id,
            open_warnings,
            actor_tx: Mutex::new(Some(actor_tx)),
            active_jobs,
            pool_active_jobs,
            healthy,
            runtime,
            module_queries: ModuleQueryCache::with_capacity(cache_cap),
            actor_scope,
            process_tracker,
        }))
    }

    /// Process one module query through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query(
        &self,
        imports: Vec<String>,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQuery {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            source,
            query,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Process one module-query batch through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query_batch(
        &self,
        imports: Vec<String>,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQueryBatch {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            source,
            selectors,
            budgets,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Inspect one declaration through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn inspect_declaration(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationInspectionRequest,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerDeclarationInspectionResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationInspection {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            request,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Run a group of bounded declaration searches through this project's
    /// serialized worker actor, against one session.
    ///
    /// The unit is a group rather than a single search because its one caller
    /// — `search_for_proof` — always has several: it derives up to six queries
    /// from a single goal and needs all of them to rank. Issued one at a time
    /// those were N mailbox hops and, worse, N session opens, and every open
    /// re-runs `importModules` over the whole closure. Results come back in
    /// request order.
    ///
    /// Worth the widened signature: `benches/search_for_proof.rs` on the
    /// fixture went from **2.71 s to 1.41 s** per warm call — the six session
    /// opens were about half of the most expensive tool in the surface, and the
    /// saving scales with the import closure, so a Mathlib-scale project gains
    /// more than this fixture does.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or worker
    /// execution fails. A failure of any single search fails the group: they
    /// share a session, so a worker fault that kills one has already invalidated
    /// the rest.
    pub(crate) async fn search_declarations(
        &self,
        imports: Vec<String>,
        requests: Vec<LeanWorkerDeclarationSearch>,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationSearch {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            requests,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Try proof fragments in-memory through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn attempt_proof(
        &self,
        imports: Vec<String>,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerProofAttemptResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ProofAttempt {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Verify one declaration in-memory through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerification {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Verify several declarations in one in-memory source snapshot through
    /// this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration_batch(
        &self,
        imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
        admission_deadline: Instant,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerificationBatch {
            meta: self.job_meta(imports, RetryPolicy::Never, admission_deadline),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    pub(crate) async fn native_compatibility(&self, admission_deadline: Instant) -> Result<ProjectCall<()>> {
        let (reply, rx) = oneshot::channel();
        self.enqueue(
            ProjectMessage::NativeCompatibility {
                meta: self.job_meta(Vec::new(), RetryPolicy::Never, admission_deadline),
                reply,
            },
            rx,
        )
        .await
    }

    fn job_meta(&self, imports: Vec<String>, retry_policy: RetryPolicy, admission_deadline: Instant) -> JobMeta {
        let created_at = Instant::now();
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        self.pool_active_jobs.fetch_add(1, Ordering::AcqRel);
        JobMeta {
            file_label: None,
            queue: Arc::new(QueueDeadline::new(admission_deadline)),
            import_fingerprint: import_fingerprint(&imports),
            imports,
            _created_at: created_at,
            queued_at: Instant::now(),
            correlation_id: uuid::Uuid::new_v4(),
            retry_policy,
            _active_job: ActiveJobGuard {
                active_jobs: Arc::clone(&self.active_jobs),
                pool_active_jobs: Arc::clone(&self.pool_active_jobs),
            },
            cancellation: CallCancellation::current(),
            granted_at: crate::project_lock::granted_at(&self.canonical_root),
        }
    }

    async fn enqueue<T>(
        &self,
        message: ProjectMessage,
        mut reply_rx: oneshot::Receiver<Result<ProjectCall<T>>>,
    ) -> Result<ProjectCall<T>>
    where
        T: Send + 'static,
    {
        let queue = Arc::clone(&message.meta().queue);
        let cancellation = message.meta().cancellation.clone();
        let operation_id = message.meta().correlation_id.to_string();
        let operation = cancellation.begin_operation_for(operation_id.clone());
        let mut on_drop = cancellation.cancel_on_drop();
        if cancellation.is_cancelled() {
            operation.not_started();
            return Err(self.unavailable("request_cancelled", false, false));
        }
        let project_info = self.worker_error_context(message.imports());
        let tx = self
            .actor_tx
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| self.unavailable("project actor is stopped", false, false))?;
        // Tokio's reservation queue is FIFO. Keep one reservation future alive
        // while waiting, rather than racing repeated try_send calls. Cancellation
        // or expiry drops the unsent message and its activity guard.
        match reserve_mailbox(
            &tx,
            &cancellation,
            queue.deadline.saturating_duration_since(Instant::now()),
        )
        .await
        {
            Ok(permit) => {
                permit.send(message);
            }
            Err(reason) => {
                if reason == "mailbox_closed" {
                    self.shutdown();
                }
                operation.not_started();
                return Err(ServerError::worker_unavailable(WorkerUnavailable {
                    retryable: reason != "request_cancelled",
                    worker_restarted: false,
                    worker_stop_receipt: None,
                    reason: reason.to_owned(),
                    ..project_info
                }));
            }
        }

        let reply = tokio::select! {
            biased;
            reply = &mut reply_rx => reply,
            () = cancellation.cancelled() => {
                if queue.expire() {
                    operation.not_started();
                    return Err(self.unavailable("request_cancelled", false, false));
                }
                // A running worker owns cleanup; do not report cancellation
                // complete until its reply confirms that cleanup finished.
                reply_rx.await
            }
            () = tokio::time::sleep_until(queue.deadline.into()) => {
                if queue.expire() {
                    // The actor shares this state, so it cannot dispatch later.
                    operation.not_started();
                    on_drop.disarm();
                    return Err(self.unavailable("queue_wait_timeout", true, false));
                }
                // Dispatch won the race. Its own execution timeout now applies.
                reply_rx.await
            }
        };
        let result = match reply {
            Ok(result) => result,
            Err(_) => {
                self.shutdown();
                Err(self.unavailable("mailbox_closed_before_reply", true, false))
            }
        };
        match &result {
            Ok(_) => operation.returned(),
            Err(_) if queue.expire() => operation.not_started(),
            Err(ServerError::WorkerUnavailable(failure)) => {
                if let Some(receipt) = &failure.worker_stop_receipt
                    && receipt.operation_id == operation_id
                    && receipt.worker_session_id == self.session_id
                {
                    operation.stopped(receipt.clone());
                } else if failure.runtime.final_attempt == FinalAttemptStatus::Completed {
                    // This operation returned. The unavailable payload carries
                    // no proof result.
                    operation.returned();
                } else {
                    operation.unknown();
                }
            }
            Err(_) => operation.unknown(),
        }
        on_drop.disarm();
        result
    }

    pub(crate) fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub(crate) fn toolchain(&self) -> &str {
        &self.toolchain
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn module_query_cache(&self) -> &ModuleQueryCache {
        &self.module_queries
    }

    #[must_use]
    pub(crate) fn freshness(&self, request_imports: &[String]) -> Freshness {
        Freshness {
            elaboration_options: None,
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: request_imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
            toolchain_advisories: self.open_warnings.clone(),
        }
    }

    #[must_use]
    pub(crate) fn runtime_facts(&self) -> RuntimeFacts {
        self.runtime.lock().facts()
    }

    pub(crate) fn shutdown(&self) {
        self.healthy.store(false, Ordering::Release);
        let _ = self.actor_tx.lock().take();
    }

    /// Whether the actor has ended and every worker process it started has a
    /// confirmed exit, the same test that releases its worker slot.
    pub(crate) fn has_exited(&self) -> bool {
        self.actor_scope.strong_count() == 0 && self.process_tracker.active_processes() == 0
    }

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.actor_tx.lock().as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// The generation of this project's worker while the actor is healthy.
    /// A cached answer is good only while the generation that computed it
    /// lives. The actor notices a dead child within one liveness poll.
    pub(crate) fn live_generation(&self) -> Option<u64> {
        self.is_healthy().then(|| self.runtime.lock().worker_generation)
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.outstanding_jobs() == 0
    }

    pub(crate) fn outstanding_jobs(&self) -> usize {
        self.active_jobs.load(Ordering::Acquire)
    }

    pub(crate) fn begin_maintenance(&self, session_id: &str) -> Option<ActiveJobGuard> {
        if self.session_id != session_id || !self.is_healthy() {
            return None;
        }
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        self.pool_active_jobs.fetch_add(1, Ordering::AcqRel);
        Some(ActiveJobGuard {
            active_jobs: Arc::clone(&self.active_jobs),
            pool_active_jobs: Arc::clone(&self.pool_active_jobs),
        })
    }

    fn unavailable(&self, reason: impl Into<String>, retryable: bool, worker_restarted: bool) -> ServerError {
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            reason: reason.into(),
            ..self.worker_error_context(&[])
        })
    }

    fn worker_error_context(&self, imports: &[String]) -> WorkerUnavailable {
        let snapshot = self.runtime.lock().clone();
        let runtime = snapshot.facts();
        WorkerUnavailable {
            retryable: true,
            worker_restarted: false,
            worker_stop_receipt: None,
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
            worker_generation: snapshot.worker_generation,
            reason: String::new(),
            blocked_by: None,
            restart_cause: snapshot.last_restart.as_ref().map(|event| event.cause.clone()),
            rss_kib: snapshot.rss_kib,
            limit_kib: None,
            retry_after_millis: None,
            restarts_in_window: None,
            window_millis: None,
            runtime,
            toolchain_advisories: self.open_warnings.clone(),
        }
    }
}

impl Drop for LeanProject {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Clone)]
struct ActorConfig {
    maintenance: crate::broker::MaintenanceAccess,
    lake_root: PathBuf,
    manifest_hash: String,
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    prepared_host_artifacts: Option<lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts>,
    session_id: String,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    healthy: Arc<AtomicBool>,
    startup_uncertain: Arc<AtomicBool>,
    process_tracker: LeanWorkerProcessTracker,
    lean_max_memory_kib: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
    import_residue_budget_bytes: u64,
    import_residue_soft_bytes: u64,
    session_pool_capacity: usize,
    /// The tokio runtime the actor may borrow to wait on its mailbox with a
    /// timeout, captured where [`LeanProject::open`] runs inside one.
    ///
    /// `None` in a synchronous test context, which degrades to the mailbox-empty
    /// check alone — the same degradation the broker's reaper already accepts.
    /// The handle is used *only* to wrap the receive; no Lean session ever
    /// crosses it.
    tokio_handle: Option<tokio::runtime::Handle>,
    /// Open-time toolchain advisories (unknown pin, missing sidecar, no smoke
    /// record). The actor carries them so a `runtime_unavailable` it produces
    /// after worker death still flags a suspect worker. Mirrors
    /// [`LeanProject::open_warnings`]; both come from the one
    /// [`WorkerBinary::resolve_ready`] verdict at open.
    toolchain_advisories: Vec<String>,
}

/// Resolve one spawnable worker before taking broker admission resources.
///
/// The worker comes from the worker directory of the project's own Lake root
/// ([`crate::toolchain::workers_dir`]). Every refusal is one line that ends
/// with the command that fixes it.
pub(crate) fn resolve_worker(meta: &LakeProjectMeta) -> Result<(PathBuf, PathBuf, Vec<String>)> {
    let toolchain_id = ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
    let dir = crate::toolchain::workers_dir(&meta.canonical_root, None);
    let fix = crate::toolchain::install_fix(&meta.canonical_root);
    let worker = format!("worker for leanprover/lean4:{toolchain_id} in {}", dir.display());
    let (worker_path, lean_sysroot, open_warnings) = match WorkerBinary::resolve_ready(&toolchain_id, &dir, &fix) {
        Readiness::Ready {
            worker,
            lean_sysroot,
            note,
        } => (worker.path, lean_sysroot, note.into_iter().collect()),
        Readiness::UnknownPin {
            pin,
            worker,
            lean_sysroot,
        } => (
            worker.path,
            lean_sysroot,
            vec![format!(
                "lean-toolchain pins {pin}, which is not a recognized lean-rs supported version \
                 (e.g. a nightly); proceeding, but the host cannot vouch for ABI compatibility"
            )],
        ),
        Readiness::Unsupported { window, nearest } => {
            return Err(ServerError::BadProject(format!(
                "lean-toolchain pins {toolchain_id}, outside the lean-rs supported window {window}; \
                 nearest supported: {nearest}. Pin a supported toolchain (or bump lean-rs) and reopen."
            )));
        }
        Readiness::Stale { .. } => {
            return Err(ServerError::BadProject(format!(
                "{worker} was built against another lean.h than the toolchain now has; run: {fix}"
            )));
        }
        Readiness::Incompatible {
            worker_protocol,
            host_protocol,
            ..
        } => {
            return Err(ServerError::IncompatibleWorker {
                message: format!(
                    "{worker} uses protocol {worker_protocol}, but this host requires protocol {host_protocol}"
                ),
                recovery_command: fix,
            });
        }
        Readiness::Unusable { detail, .. } => {
            CallCancellation::current().begin_operation().not_started();
            let detail = detail.lines().next().unwrap_or_default().trim();
            return Err(ServerError::BadProject(format!(
                "{worker} is unusable ({detail}); run: {fix}"
            )));
        }
        Readiness::NotInstalled { .. } => {
            return Err(ServerError::BadProject(format!(
                "no worker for leanprover/lean4:{toolchain_id} in {}; run: {fix}",
                dir.display()
            )));
        }
        Readiness::ToolchainNotInstalled { toolchain, elan_dir } => {
            return Err(ServerError::BadProject(format!(
                "elan toolchain leanprover/lean4:{toolchain} is not installed (expected {}); \
                 run: elan toolchain install leanprover/lean4:{toolchain}",
                elan_dir.display()
            )));
        }
    };
    tracing::debug!(
        toolchain = %toolchain_id,
        worker = %worker_path.display(),
        sysroot = %lean_sysroot.display(),
        "resolved ready worker binary"
    );
    Ok((worker_path, lean_sysroot, open_warnings))
}

impl ActorConfig {
    /// Resolve the pinned toolchain into a spawnable config plus any
    /// open-time provenance advisories. All version-drift situations collapse
    /// into the one [`WorkerBinary::resolve_ready`] verdict: hard failures
    /// become a typed [`ServerError::BadProject`] carrying the corrective
    /// command; soft ones (unknown pin, missing sidecar) ride along as
    /// warnings the project surfaces in every envelope.
    fn from_meta(
        meta: &LakeProjectMeta,
        session_id: String,
        runtime: Arc<Mutex<RuntimeSnapshot>>,
        healthy: Arc<AtomicBool>,
        runtime_config: ProjectRuntimeConfig,
    ) -> Result<(Self, Vec<String>)> {
        let (worker_path, lean_sysroot, open_warnings) = resolve_worker(meta)?;
        let config = Self {
            maintenance: crate::broker::MaintenanceAccess::default(),
            lake_root: meta.canonical_root.clone(),
            manifest_hash: meta.manifest_hash.clone(),
            toolchain_label: meta.toolchain.clone(),
            worker_path,
            lean_sysroot,
            prepared_host_artifacts: None,
            session_id,
            runtime,
            healthy,
            startup_uncertain: Arc::new(AtomicBool::new(false)),
            process_tracker: LeanWorkerProcessTracker::new(),
            lean_max_memory_kib: runtime_config.lean_max_memory_kib(),
            request_timeout_millis: runtime_config.request_timeout_millis(),
            mailbox_capacity: runtime_config.mailbox_capacity(),
            max_restarts_per_window: runtime_config.max_restarts_per_window(),
            restart_window: runtime_config.restart_window(),
            import_residue_budget_bytes: runtime_config.import_residue_budget_bytes(),
            import_residue_soft_bytes: runtime_config.import_residue_soft_bytes(),
            session_pool_capacity: runtime_config.session_pool_capacity(),
            tokio_handle: tokio::runtime::Handle::try_current().ok(),
            toolchain_advisories: open_warnings.clone(),
        };
        Ok((config, open_warnings))
    }
}

/// One import profile the child may be holding.
struct ImportProfile {
    fingerprint: String,
    /// The profile's imports, kept so a pre-warm after an idle cycle can reopen
    /// it. The fingerprint is `imports.join("\n")` and could be split back, but
    /// that makes the pre-warm depend on the fingerprint's spelling; this list is
    /// what the child was actually asked for.
    imports: Vec<String>,
    /// When this profile was last served, so an idle cycle only pre-warms a
    /// profile a returning caller is plausibly still working in.
    last_served: Instant,
}

/// The finished-build count under which a worker generation first loaded
/// imports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoadedAt {
    generation: u64,
    builds_finished: u64,
}

impl LoadedAt {
    /// Whether a job whose read turn saw `granted_at` must not reuse
    /// `generation`: a build finished after that generation loaded. A record
    /// for another generation says nothing about this one.
    const fn stale_for(self, generation: u64, granted_at: u64) -> bool {
        self.generation == generation && granted_at > self.builds_finished
    }
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    worker_generation_base: u64,
    last_restart: Option<RuntimeRestartEvent>,
    /// The import profiles the live worker child may still be holding, MRU at
    /// the back, bounded by [`ProjectRuntimeConfig::session_pool_capacity`] so
    /// it mirrors the child's own session pool.
    ///
    /// A *list* because the child pools sessions: it holds several imported
    /// environments at once, so "the profile the live session was opened with"
    /// is no longer a single value.
    imports_seen: Vec<ImportProfile>,
    /// The finished-build count under which the live generation first loaded
    /// imports. A record for an older generation is ignored.
    loaded_at: Option<LoadedAt>,
    profile_switch_count: u64,
    last_rss_kib: Option<u64>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    abnormal_restart_times: VecDeque<Instant>,
    restart_stats: RestartStats,
    /// Whether this quiet period has already taken its idle cycle.
    ///
    /// One is all a quiet period can profitably take. The pre-warm that follows
    /// a cycle re-imports, which puts the fresh child's residue back above zero
    /// and, under a budget small enough, straight back over the soft threshold —
    /// so without this the actor would cycle, pre-warm, and cycle again every
    /// grace interval, discarding each pre-warm with nothing served in between.
    /// Cleared by the next served call, which is what makes the *next* cycle
    /// worth something.
    cycled_while_idle: bool,
}

impl ProjectActorState {
    fn handle_message(&mut self, message: ProjectMessage) {
        // A call was served, so the next quiet period earns a fresh idle cycle.
        self.cycled_while_idle = false;
        match message {
            ProjectMessage::NativeCompatibility { meta, reply } => {
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    let _session = handle.open_session_with_imports(imports, Some(cancellation), None)?;
                    Ok(())
                });
                let _ = reply.send(result);
            }
            ProjectMessage::ModuleQuery {
                meta,
                source,
                query,
                options,
                reply,
            } => {
                let mut meta = meta;
                meta.file_label = Some(options.file_label.clone());
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.process_module_query_with_imports(
                        imports,
                        &source,
                        &query,
                        &options,
                        Some(cancellation),
                        None,
                    )
                });
                let _ = reply.send(result);
            }
            ProjectMessage::ModuleQueryBatch {
                meta,
                source,
                selectors,
                budgets,
                options,
                reply,
            } => {
                let mut meta = meta;
                meta.file_label = Some(options.file_label.clone());
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.process_module_query_batch_with_imports(
                        imports,
                        &source,
                        &selectors,
                        &budgets,
                        &options,
                        Some(cancellation),
                        None,
                    )
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationInspection { meta, request, reply } => {
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.inspect_declaration_with_imports(imports, &request, Some(cancellation), None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationSearch { meta, requests, reply } => {
                // One session for the whole group. `search_declarations_with_imports`
                // opens and drops a session per call, and every open re-imports,
                // so the per-search cost here is a worker round trip rather than
                // an import. The session borrows `&mut handle` and dies inside
                // the closure, so nothing escapes the actor's stack frame.
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    let mut session = handle.open_session_with_imports(imports, Some(cancellation), None)?;
                    requests
                        .iter()
                        .map(|request| session.search_declarations(request, Some(cancellation), None))
                        .collect()
                });
                let _ = reply.send(result);
            }
            ProjectMessage::ProofAttempt {
                meta,
                request,
                options,
                reply,
            } => {
                let mut meta = meta;
                meta.file_label = Some(options.file_label.clone());
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.attempt_proof_with_imports(imports, &request, &options, Some(cancellation), None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerification {
                meta,
                request,
                options,
                reply,
            } => {
                let mut meta = meta;
                meta.file_label = Some(options.file_label.clone());
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.verify_declaration_with_imports(imports, &request, &options, Some(cancellation), None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerificationBatch {
                meta,
                request,
                options,
                reply,
            } => {
                let mut meta = meta;
                meta.file_label = Some(options.file_label.clone());
                let result = self.run_job(meta, |handle, imports, cancellation| {
                    handle.verify_declaration_batch_with_imports(imports, &request, &options, Some(cancellation), None)
                });
                let _ = reply.send(result);
            }
        }
    }

    /// Observe owned process state without sending a health request.
    /// Return whether an active call must retain lifecycle uncertainty.
    fn poll_worker_liveness(&mut self) -> bool {
        match self.handle.status() {
            Ok(lean_rs_worker_parent::LeanWorkerStatus::Running) => {
                if self.handle.cleanup_pending() {
                    self.config.healthy.store(false, Ordering::Release);
                    return true;
                }
                false
            }
            Ok(lean_rs_worker_parent::LeanWorkerStatus::Exited(_)) => {
                self.config.healthy.store(false, Ordering::Release);
                false
            }
            Err(_) => {
                self.config.healthy.store(false, Ordering::Release);
                self.config.startup_uncertain.store(true, Ordering::Release);
                true
            }
        }
    }

    fn run_job<R>(
        &mut self,
        meta: JobMeta,
        job: impl Fn(
            &mut LeanWorkerHostHandle,
            Vec<String>,
            &LeanWorkerCancellationToken,
        ) -> std::result::Result<R, LeanWorkerError>,
    ) -> Result<ProjectCall<R>> {
        if meta.cancellation.is_cancelled() {
            return Err(self.shutdown_unavailable(&meta, "request_cancelled"));
        }
        // Runs on the project's dedicated actor thread (no async), so an entered
        // span is correct and ties every nested worker/recycle log to this call.
        let _span = tracing::debug_span!(
            "job",
            session_id = %self.config.session_id,
            imports = meta.imports.len(),
            queue_wait_millis = millis_u64(meta.queued_at.elapsed()),
        )
        .entered();
        let queue_wait_millis = millis_u64(meta.queued_at.elapsed());
        let generation_before = self.observed_generation();
        self.note_import_profile_switch(&meta);
        let job_started = Instant::now();
        let cpu_baseline = self.cpu_baseline();
        self.note_import_profile(&meta.import_fingerprint, &meta.imports);
        let mut call_restart: Option<RuntimeRestartEvent> = self.cycle_if_build_finished(&meta)?;
        let established = crate::broker::MaintenanceAccess::native_contract(&self.config.toolchain_label)
            .map_err(ServerError::BadProject)?;
        let expectation =
            crate::runtime_compatibility::session_expectation(&self.config.toolchain_label, established.as_ref())
                .map_err(ServerError::BadProject)?;
        self.handle
            .set_host_runtime_expectation(expectation)
            .map_err(map_worker_err)?;
        let mut lifecycle_baseline = self.handle.lifecycle_snapshot();

        let max_retries = meta.retry_policy.retries();
        let mut retry_count = 0_u32;
        let mut previous_attempt_failure = None;
        let mut final_attempt = FinalAttemptStatus::Interrupted;
        let mut worker_stop_receipt = None;
        // Every submitted job uses Never: a stopped worker does not authorize
        // replaying Lean IO. Outcomes share this finalization point so timing
        // and lifecycle facts describe the one submitted attempt.
        let exit = loop {
            if meta.cancellation.is_cancelled() {
                return Err(self.shutdown_unavailable(&meta, "request_cancelled"));
            }
            let checkpoint = self.handle.request_checkpoint();
            let result = job(&mut self.handle, meta.imports.clone(), meta.cancellation.worker());
            if let Err(error) = &result {
                // Observing status never dispatches work. In particular, an old
                // request's receipt cannot settle a failed replacement startup.
                let metadata_refusal =
                    self.handle.host_session_refusal_since(&checkpoint) == Some("worker_host_runtime_metadata");
                let lifecycle_uncertain = self.poll_worker_liveness();
                // This exact marker exists only after a typed metadata response
                // returned and the parent rejected compatibility. Later cleanup
                // cannot turn that pre-body refusal into unknown execution.
                let returned = metadata_refusal
                    || (matches!(error, LeanWorkerError::Worker { .. })
                        && !lifecycle_uncertain
                        && self.config.healthy.load(Ordering::Acquire)
                        && !self.config.startup_uncertain.load(Ordering::Acquire));
                if let (Some(file_label), Some(operation)) = (
                    meta.file_label.as_ref(),
                    self.handle.host_session_refusal_since(&checkpoint),
                ) {
                    let stage = match operation {
                        "open_worker_session" => Some("host_session_import"),
                        "worker_host_runtime_metadata" => Some("host_runtime_validation"),
                        _ => None,
                    };
                    if let Some(stage) = stage {
                        meta.cancellation
                            .target_check_not_started(crate::envelope::TargetCheckNotStarted {
                                disposition: "not_started".to_owned(),
                                file_label: file_label.clone(),
                                stage: stage.to_owned(),
                            });
                    }
                }
                if metadata_refusal {
                    self.config.healthy.store(false, Ordering::Release);
                }
                if returned {
                    // A typed terminal error from the healthy owned child is
                    // known to have returned. It is not a proof result or exit.
                    final_attempt = FinalAttemptStatus::Completed;
                } else {
                    let deferred_stop = self.defer_stop_observation(
                        &meta,
                        self.handle.pending_request_stop_since(&checkpoint),
                        "main",
                        self.worker_generation_base,
                        stop_cause(error, &self.handle.lifecycle_snapshot()),
                    );
                    if lifecycle_uncertain && !deferred_stop {
                        meta.cancellation.mark_uncertain();
                    }
                }
                worker_stop_receipt = self.handle.confirmed_request_stop_since(&checkpoint).map(|receipt| {
                    confirmed_stop_receipt(
                        &self.config.session_id,
                        meta.correlation_id,
                        "main",
                        self.worker_generation_base,
                        receipt,
                        stop_cause(error, &self.handle.lifecycle_snapshot()),
                    )
                });
            }
            match result {
                Ok(value) => {
                    let restarts = self
                        .handle
                        .lifecycle_snapshot()
                        .restarts
                        .saturating_sub(lifecycle_baseline.restarts);
                    if let Some(event) = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?
                    {
                        call_restart = Some(event);
                    }
                    if meta.cancellation.is_cancelled() {
                        break JobExit::Unavailable {
                            reason: "request_cancelled".to_owned(),
                            retryable: false,
                            worker_restarted: self.observed_generation() > generation_before,
                            cause: Some(RestartCause::Cancelled),
                            limit_kib: None,
                        };
                    }
                    final_attempt = if restarts > 1 {
                        call_restart = Some(restart_event(
                            RestartCause::WorkerInternal,
                            "multiple worker restarts during final attempt; result unavailable",
                            self.observed_generation(),
                            self.last_rss_kib,
                            None,
                        ));
                        FinalAttemptStatus::Unknown
                    } else if call_restart
                        .as_ref()
                        .is_some_and(crate::diagnosis::restart_disrupts_result)
                    {
                        FinalAttemptStatus::Interrupted
                    } else {
                        FinalAttemptStatus::Completed
                    };
                    // Sampled for `RuntimeFacts.rss_kib`, which is reporting
                    // only: no threshold reads it, and nothing restarts on it.
                    self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
                    break JobExit::Done(value);
                }
                Err(_err) if meta.cancellation.is_cancelled() => {
                    if worker_stop_receipt.is_none() {
                        self.config.healthy.store(false, Ordering::Release);
                    }
                    call_restart = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?;
                    break JobExit::Unavailable {
                        reason: "request_cancelled".to_owned(),
                        retryable: false,
                        worker_restarted: self.observed_generation() > generation_before,
                        cause: Some(RestartCause::Cancelled),
                        limit_kib: None,
                    };
                }
                Err(err) if worker_error_is_recoverable_death(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?;
                    let first_reason = err.to_string();
                    previous_attempt_failure =
                        Some(self.rebuild_after_worker_death(first_reason, worker_death_cause(&err), &meta)?);
                    call_restart = None;
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_recoverable_death(&err) => {
                    if let Some(event) = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?
                    {
                        call_restart = Some(event);
                    }
                    break JobExit::Unavailable {
                        reason: format!("worker_died: {err}"),
                        retryable: true,
                        worker_restarted: self.observed_generation() > generation_before,
                        cause: Some(worker_death_cause(&err)),
                        limit_kib: None,
                    };
                }
                Err(err) if worker_error_is_session_missing(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?;
                    previous_attempt_failure = Some(self.rebuild_after_worker_death(
                        format!("session_missing: {err}"),
                        RestartCause::SessionMissing,
                        &meta,
                    )?);
                    call_restart = None;
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_session_missing(&err) => {
                    if let Some(event) = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?
                    {
                        call_restart = Some(event);
                    }
                    break JobExit::Unavailable {
                        reason: format!("session_missing: {err}"),
                        retryable: true,
                        worker_restarted: self.observed_generation() > generation_before,
                        cause: Some(RestartCause::SessionMissing),
                        limit_kib: None,
                    };
                }
                Err(LeanWorkerError::RssHardLimitExceeded {
                    operation,
                    current_kib,
                    limit_kib,
                    ..
                }) => {
                    if let Some(event) = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?
                    {
                        call_restart = Some(event);
                    }
                    break JobExit::Unavailable {
                        reason: format!(
                            "rss_hard_limit_exceeded operation={operation} current_kib={current_kib} limit_kib={limit_kib}"
                        ),
                        retryable: false,
                        worker_restarted: true,
                        cause: Some(RestartCause::RssHardLimit),
                        limit_kib: Some(limit_kib),
                    };
                }
                Err(err) if matches!(err, LeanWorkerError::Timeout { .. }) => {
                    if let Some(event) = self
                        .account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?
                    {
                        call_restart = Some(event);
                    }
                    break JobExit::Unavailable {
                        reason: format!("timeout: {err}"),
                        retryable: true,
                        worker_restarted: self.observed_generation() > generation_before,
                        cause: Some(RestartCause::Timeout),
                        limit_kib: None,
                    };
                }
                Err(err) => {
                    // An infrastructure failure may leave an owned generation
                    // unresolved. Seal this actor before another job can spawn.
                    if matches!(
                        err,
                        LeanWorkerError::Kill { .. }
                            | LeanWorkerError::Wait { .. }
                            | LeanWorkerError::WaitTimeout { .. }
                            | LeanWorkerError::Setup { .. }
                    ) {
                        self.config.healthy.store(false, Ordering::Release);
                    }
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)
                        .map_err(|error| with_stop_receipt(error, worker_stop_receipt.clone()))?;
                    break JobExit::Unavailable {
                        reason: format!("worker_operation_failed: {err}"),
                        retryable: false,
                        worker_restarted: self.observed_generation() > generation_before,
                        cause: None,
                        limit_kib: None,
                    };
                }
            }
        };

        // A retry or restart may have loaded imports on a new generation,
        // still under this job's read turn.
        if let Some(granted_at) = meta.granted_at {
            self.note_loaded_at(granted_at);
        }
        // The one finalization point: elapsed/CPU are sampled after the loop,
        // and the facts are built and published exactly once per call.
        let call_elapsed_millis = millis_u64(job_started.elapsed());
        let call_cpu_millis = self.cpu_delta_since(cpu_baseline);
        let mut runtime = self.runtime_facts(
            &meta,
            generation_before,
            retry_count,
            queue_wait_millis,
            call_elapsed_millis,
            call_cpu_millis,
            call_restart,
        );
        runtime.final_attempt = final_attempt;
        runtime.previous_attempt_failure = previous_attempt_failure;
        if matches!(&exit, JobExit::Done(_)) {
            let actual = self.handle.host_runtime_metadata().ok_or_else(|| {
                ServerError::BadProject("actual host runtime metadata missing after completed native call".into())
            })?;
            let proposed =
                crate::runtime_compatibility::contract_from_loaded(actual).map_err(ServerError::BadProject)?;
            let (instance, contract) = self
                .config
                .maintenance
                .bind_native_contract(&self.config.toolchain_label, &proposed)
                .map_err(ServerError::BadProject)?;
            runtime.native_compatibility = Some(
                crate::runtime_compatibility::evidence(
                    contract,
                    instance,
                    &self.config.lake_root.to_string_lossy(),
                    &self.config.toolchain_label,
                    &self.config.session_id,
                    self.observed_generation(),
                    &self.handle.runtime_metadata(),
                    actual,
                )
                .map_err(ServerError::BadProject)?,
            );
        }
        self.publish_runtime(&runtime);
        match exit {
            JobExit::Done(value) => {
                tracing::debug!(
                    retry_count,
                    rss_kib = ?runtime.rss_kib,
                    worker_generation = runtime.worker_generation,
                    "job complete"
                );
                Ok(ProjectCall::new(value, runtime))
            }
            JobExit::Unavailable {
                reason,
                retryable,
                worker_restarted,
                cause,
                limit_kib,
            } => {
                let mut error =
                    self.worker_unavailable_for(&meta, reason, retryable, worker_restarted, cause, limit_kib, None);
                if let ServerError::WorkerUnavailable(failure) = &mut error {
                    failure.runtime = runtime;
                    failure.worker_stop_receipt = worker_stop_receipt;
                }
                Err(error)
            }
        }
    }

    /// Retain one exact unresolved direct child for an eventual owned reap.
    /// A receipt already confirmed before another cleanup failure cannot settle
    /// that independent replacement uncertainty.
    fn defer_stop_observation(
        &self,
        meta: &JobMeta,
        pending: Option<lean_rs_worker_parent::LeanWorkerPendingRequestStop>,
        scope: &'static str,
        generation_base: u64,
        cause: &'static str,
    ) -> bool {
        let Some(pending) = pending else { return false };
        let unresolved = pending.confirmed_stop().is_none();
        let session = self.config.session_id.clone();
        let id = meta.correlation_id;
        meta.cancellation.defer_stop_state(
            id.to_string(),
            Arc::new(move || {
                use crate::cancellation::DeferredStopState;
                if let Some(receipt) = pending.confirmed_stop() {
                    DeferredStopState::Stopped(confirmed_stop_receipt(
                        &session,
                        id,
                        scope,
                        generation_base,
                        &receipt,
                        cause,
                    ))
                } else {
                    DeferredStopState::Pending
                }
            }),
        );
        unresolved
    }

    /// Replace the worker when a build finished after it loaded imports.
    ///
    /// A worker session's environment is a snapshot taken at import, and the
    /// child pools sessions, so `.olean` files written later are invisible to
    /// every profile it holds. Each job carries the finished-build count of its
    /// read turn; the actor keeps the count under which the live generation
    /// first loaded. A higher count means a write turn ended in between, so the
    /// job runs on a fresh worker instead, which is why `artifacts_rebuilt` is
    /// not an execution taint.
    ///
    /// A build stops every worker of the project before Lake runs, so no build
    /// can finish under a live generation and this check is a backstop. A job
    /// sent without a read turn is not checked.
    fn cycle_if_build_finished(&mut self, meta: &JobMeta) -> Result<Option<RuntimeRestartEvent>> {
        let Some(granted_at) = meta.granted_at else {
            return Ok(None);
        };
        let generation = self.observed_generation();
        if !self
            .loaded_at
            .is_some_and(|loaded| loaded.stale_for(generation, granted_at))
        {
            self.note_loaded_at(granted_at);
            return Ok(None);
        }
        let reason = format!(
            "artifacts_rebuilt imports={} build_finished_since_load",
            meta.imports.len()
        );
        self.record_restart_or_stop(RestartCause::ArtifactsRebuilt, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        self.handle.cycle().map_err(map_worker_err)?;
        self.forget_pooled_import_profiles();
        self.note_loaded_at(granted_at);
        let event = restart_event(
            RestartCause::ArtifactsRebuilt,
            reason,
            self.observed_generation(),
            self.last_rss_kib,
            None,
        );
        self.record_restart(event.clone());
        Ok(Some(event))
    }

    /// Record that the live generation loads imports under `builds_finished`,
    /// unless it already has a record. The first load is the one that counts.
    fn note_loaded_at(&mut self, builds_finished: u64) {
        let generation = self.observed_generation();
        if self.loaded_at.is_none_or(|loaded| loaded.generation != generation) {
            self.loaded_at = Some(LoadedAt {
                generation,
                builds_finished,
            });
        }
    }

    /// The profile the most recent job ran with, or `None` before the first job.
    ///
    /// Reported as the `import_profile` runtime fact and used to count switches.
    /// Both want "most recent", which is the back of the MRU list.
    fn current_import_profile(&self) -> Option<&str> {
        self.imports_seen.last().map(|held| held.fingerprint.as_str())
    }

    /// Record that `fingerprint` is now the most recently served profile.
    fn note_import_profile(&mut self, fingerprint: &str, imports: &[String]) {
        if let Some(index) = self
            .imports_seen
            .iter()
            .position(|held| held.fingerprint == fingerprint)
        {
            let mut held = self.imports_seen.remove(index);
            held.last_served = Instant::now();
            self.imports_seen.push(held);
            return;
        }
        // Bounded by the child's own session-pool capacity, so the parent's
        // picture of what the child holds cannot outgrow what it can hold.
        while self.imports_seen.len() >= self.config.session_pool_capacity {
            self.imports_seen.remove(0);
        }
        self.imports_seen.push(ImportProfile {
            fingerprint: fingerprint.to_owned(),
            imports: imports.to_vec(),
            last_served: Instant::now(),
        });
    }

    /// Cycle the child now, between requests, if imports have retained more than
    /// the soft share of the residue budget.
    ///
    /// The recycle itself is not avoidable — residue is unreclaimable, so a
    /// child that has imported enough must be replaced eventually — but *when*
    /// it lands is. Run reactively it costs the next caller a ~730 ms respawn on
    /// top of their import; run here it costs nobody anything, because every
    /// reply has already been sent and the mailbox is empty.
    ///
    /// A cheap recycle is also newly worth taking. `.olean` compacted regions
    /// are position-dependent, so the *first* import in a fresh process maps
    /// them at their preferred addresses and retains almost nothing — 0.29 GB of
    /// footprint for a 2.2 GB closure, against 2–4.5 GB for the same import
    /// performed later in a process. A fresh child is not merely emptier, its
    /// next import is far cheaper.
    ///
    /// Having cycled, it re-imports the most recently served profile into the
    /// fresh child, so the next caller does not pay for the emptiness this
    /// created. Bounded by recency: a profile nobody has touched in
    /// [`PREWARM_RECENCY`] is a guess, and a wrong guess costs a real import.
    /// Minimum quiet period for a residue cycle. Lifecycle observation is
    /// independent of the residue policy and never resets this quiet period.
    fn idle_grace(&self) -> Option<Duration> {
        self.config.tokio_handle.as_ref()?;
        self.over_soft_budget().map(|_residue| QUIESCENCE_GRACE)
    }

    /// The generation's retained residue, if it is worth cycling for.
    ///
    /// Two conditions, not one. The threshold is the obvious half. The other is
    /// that the generation has actually retained something: a child that has
    /// imported nothing has nothing a cycle could reclaim, so replacing it is
    /// pure cost — and with a soft budget of zero (a budget small enough that
    /// 60% of it rounds to nothing) the threshold alone would be satisfied by
    /// every freshly spawned child, cycling forever and importing never.
    fn over_soft_budget(&self) -> Option<u64> {
        if self.cycled_while_idle {
            return None;
        }
        let residue = self.handle.lifecycle_snapshot().import_residue_bytes;
        (residue > 0 && residue >= self.config.import_residue_soft_bytes).then_some(residue)
    }

    /// Cycle over the soft budget, having *demonstrated* that the project is
    /// quiet: the mailbox stayed empty for a whole [`QUIESCENCE_GRACE`]. That is
    /// what earns the pre-warm, which is a multi-second import at Mathlib scale
    /// and would land on the next caller's latency if taken mid-burst.
    fn cycle_on_quiescence(&mut self) {
        self.cycle_over_soft_budget(true);
    }

    /// Cycle over the soft budget with nothing more than an empty mailbox to go
    /// on. Used only where no tokio handle is available to arm the quiescence
    /// timer — a synchronous embedder or test — so idle cycling degrades to
    /// moving the respawn off the request path rather than disappearing. No
    /// pre-warm: an empty mailbox one instant after a reply is not evidence of a
    /// pause, and a wrong guess costs a real import.
    fn cycle_after_reply(&mut self) {
        self.cycle_over_soft_budget(false);
    }

    fn cycle_over_soft_budget(&mut self, quiesced: bool) {
        let Some(residue) = self.over_soft_budget() else {
            return;
        };
        // Idle recycling and prewarming can start a child and elaborate Lean.
        // They participate in the same atomic admission/retirement boundary.
        let Some(activity) = self
            .config
            .maintenance
            .try_begin(&self.config.lake_root, &self.config.session_id)
        else {
            return;
        };
        // Captured before the cycle, because the cycle invalidates every
        // remembered profile and this is the one worth paying to get back.
        let prewarm = quiesced
            .then(|| {
                self.imports_seen
                    .last()
                    .filter(|held| held.last_served.elapsed() <= PREWARM_RECENCY)
                    .map(|held| (held.fingerprint.clone(), held.imports.clone()))
            })
            .flatten();
        let reason = format!(
            "import_residue_idle residue_mib={} soft_mib={} limit_mib={}",
            residue / (1024 * 1024),
            self.config.import_residue_soft_bytes / (1024 * 1024),
            self.config.import_residue_budget_bytes / (1024 * 1024),
        );
        if let Err(err) = self.handle.cycle() {
            // There is no caller to reply to here. The supervisor retains any
            // failed replacement whose exit is unconfirmed; its historical old
            // exit cannot acknowledge shutdown or release this actor's slot.
            tracing::debug!(error = %err, "idle worker cycle failed; replacement remains supervised through shutdown");
            return;
        }
        // A full clear, not `forget_pooled_import_profiles`: that one preserves
        // the profile of the job currently being served, and there is no such
        // job here. The fresh child holds nothing.
        self.imports_seen.clear();
        // From here the cycle has happened, so this quiet period is spent
        // whether or not the pre-warm below succeeds.
        self.cycled_while_idle = true;
        let event = restart_event(
            RestartCause::ImportResidueIdle,
            reason,
            self.observed_generation(),
            self.last_rss_kib,
            None,
        );
        self.record_restart(event.clone());
        self.publish_runtime(&RuntimeFacts {
            native_compatibility: None,
            cli_file_verification: None,
            worker_generation: event.worker_generation,
            worker_restarted: true,
            retry_count: 0,
            final_attempt: FinalAttemptStatus::Unknown,
            previous_attempt_failure: None,
            queue_wait_millis: 0,
            call_elapsed_millis: 0,
            call_cpu_millis: None,
            call_restart: None,
            last_restart: Some(event),
            rss_kib: self.last_rss_kib,
            worker_lanes: 1,
            import_profile: None,
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restart_stats.total,
            restarts_by_cause: self.restart_stats.by_cause.clone(),
            import_residue_bytes: Some(self.handle.lifecycle_snapshot().import_residue_bytes),
            import_residue_limit_bytes: Some(self.config.import_residue_budget_bytes),
        });

        // The cycle relocated the respawn; this relocates the import, which at
        // Mathlib scale is the expensive half by an order of magnitude.
        let Some((fingerprint, imports)) = prewarm else {
            return;
        };
        match self.handle.open_session_with_imports(imports.clone(), None, None) {
            Ok(_session) => {
                // Record it the same way a served job would, so the *next* call
                // compares its read turn's count against the one this import
                // happened under.
                self.note_import_profile(&fingerprint, &imports);
                self.note_loaded_at(activity.builds_finished());
            }
            Err(err) => {
                // Same posture as a failed cycle: nobody is waiting, and the
                // next real call imports this itself.
                tracing::debug!(error = %err, "idle pre-warm failed; the next call will import normally");
            }
        }
    }

    /// Forget every pooled profile except the one the current job is serving.
    ///
    /// Called after the child is replaced: the new child holds nothing its
    /// predecessor held, so every remembered profile but one describes an
    /// environment that no longer exists. The exception is the current job's
    /// profile, which the caller records before the replacement and the job
    /// then re-imports into the fresh child, so it stays the profile a later
    /// idle cycle pre-warms.
    fn forget_pooled_import_profiles(&mut self) {
        if let Some(current) = self.imports_seen.pop() {
            self.imports_seen.clear();
            self.imports_seen.push(current);
        }
    }

    /// Count a change of import profile for telemetry.
    ///
    /// This used to also cycle the worker when RSS was above a soft ceiling, on
    /// the theory that an import switch would otherwise hold two imported
    /// environments at once. It now does — the child pools them deliberately,
    /// at a measured 30–50 MB per extra live environment against the ~1 GiB an
    /// import costs. What bounds retained bytes is
    /// [`ProjectRuntimeConfig::import_residue_budget_bytes`]; what backstops the
    /// heap above it is [`ProjectRuntimeConfig::lean_max_memory_kib`]. Neither is
    /// a process-level RSS reading, which mostly measured mmapped `.olean` pages.
    fn note_import_profile_switch(&mut self, meta: &JobMeta) {
        let switched = self
            .current_import_profile()
            .is_some_and(|previous| previous != meta.import_fingerprint);
        if switched {
            self.profile_switch_count = self.profile_switch_count.saturating_add(1);
        }
    }

    fn rebuild_after_worker_death(
        &mut self,
        reason: String,
        cause: RestartCause,
        meta: &JobMeta,
    ) -> Result<RuntimeRestartEvent> {
        self.record_restart_or_stop(cause, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        let next_generation = self.observed_generation().saturating_add(1);
        let (handle, _) = open_worker(&self.config)?;
        self.handle = handle;
        self.worker_generation_base = next_generation;
        self.forget_pooled_import_profiles();
        self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
        let event = restart_event(cause, reason, self.observed_generation(), self.last_rss_kib, None);
        self.record_restart(event.clone());
        Ok(event)
    }

    fn account_lifecycle_restarts_since(
        &mut self,
        before: &LeanWorkerLifecycleSnapshot,
        meta: &JobMeta,
    ) -> Result<Option<RuntimeRestartEvent>> {
        let after = self.handle.lifecycle_snapshot();
        let restarted = after.restarts.saturating_sub(before.restarts);
        if restarted == 0 {
            self.last_rss_kib = after.last_rss_kib.or(self.last_rss_kib);
            return Ok(None);
        }
        let (cause, reason) = after.last_restart_reason.as_ref().map_or_else(
            || (RestartCause::WorkerInternal, "worker_internal_restart".to_owned()),
            |reason| (restart_cause_from_worker(reason), restart_reason_text(reason)),
        );
        for _ in 0..restarted {
            self.record_restart_or_stop(cause, &reason)
                .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        }
        // The supervisor replaced the child under us — most often on its own
        // `max_imports` policy, which is exactly the pooled-profile ceiling.
        self.forget_pooled_import_profiles();
        self.last_rss_kib = after.last_rss_kib.or(self.last_rss_kib);
        let event = restart_event(cause, reason, self.observed_generation(), self.last_rss_kib, None);
        self.record_restart(event.clone());
        Ok(Some(event))
    }

    /// The single place a recycle becomes observable: tally it for frequency
    /// reporting, log it at a signal-appropriate level, and store it as the
    /// latest event. Every restart path funnels through here, so adding one is
    /// a single call. Kept distinct from [`Self::record_restart_or_stop`], which
    /// owns the orthogonal sliding-window health *policy*.
    fn record_restart(&mut self, event: RuntimeRestartEvent) {
        self.restart_stats.observe(&event.cause);
        log_restart(&event, self.restart_stats.total);
        self.last_restart = Some(event);
    }

    /// CPU milliseconds the current child has consumed since `baseline`, which
    /// is a sample taken by the same child generation. A mid-call recycle
    /// invalidates the baseline (the new child starts at zero), yielding `None`
    /// rather than a bogus delta.
    fn cpu_delta_since(&mut self, baseline: Option<CpuSampleBaseline>) -> Option<u64> {
        let baseline = baseline?;
        if baseline.worker_generation == self.observed_generation() {
            self.handle
                .cumulative_cpu_millis()
                .map(|end| end.saturating_sub(baseline.cpu_millis))
        } else {
            None
        }
    }

    /// Capture a CPU baseline for [`Self::cpu_delta_since`]: the child CPU
    /// sample plus the generation it belongs to.
    fn cpu_baseline(&mut self) -> Option<CpuSampleBaseline> {
        let worker_generation = self.observed_generation();
        self.handle.cumulative_cpu_millis().map(|cpu_millis| CpuSampleBaseline {
            worker_generation,
            cpu_millis,
        })
    }

    fn record_restart_or_stop(
        &mut self,
        cause: RestartCause,
        reason: &str,
    ) -> std::result::Result<(), RestartLimitExceeded> {
        if !cause.counts_toward_restart_limit() {
            return Ok(());
        }
        let now = Instant::now();
        while self
            .abnormal_restart_times
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) > self.config.restart_window)
        {
            self.abnormal_restart_times.pop_front();
        }
        if self.abnormal_restart_times.len() >= self.config.max_restarts_per_window {
            self.config.healthy.store(false, Ordering::Release);
            tracing::warn!(
                cause = cause.as_str(),
                restarts_in_window = self.abnormal_restart_times.len(),
                window_millis = millis_u64(self.config.restart_window),
                "restart limit exceeded; marking project unhealthy"
            );
            let message = format!(
                "restart_limit_exceeded after {} restarts in {:?}; latest: {reason}",
                self.config.max_restarts_per_window, self.config.restart_window
            );
            let event = restart_event(
                cause,
                message.clone(),
                self.observed_generation(),
                self.last_rss_kib,
                None,
            );
            self.record_restart(event.clone());
            self.publish_runtime(&RuntimeFacts {
                native_compatibility: None,
                cli_file_verification: None,
                worker_generation: self.observed_generation(),
                worker_restarted: false,
                retry_count: 0,
                final_attempt: FinalAttemptStatus::Interrupted,
                previous_attempt_failure: None,
                queue_wait_millis: 0,
                call_elapsed_millis: 0,
                call_cpu_millis: None,
                call_restart: None,
                last_restart: Some(event),
                rss_kib: self.last_rss_kib,
                worker_lanes: 1,
                import_profile: self.current_import_profile().map(str::to_owned),
                profile_switch_count: self.profile_switch_count,
                restarts_total: self.restart_stats.total,
                restarts_by_cause: self.restart_stats.by_cause.clone(),
                import_residue_bytes: Some(self.handle.lifecycle_snapshot().import_residue_bytes),
                import_residue_limit_bytes: self.config.import_residue_budget_bytes.into(),
            });
            return Err(RestartLimitExceeded {
                message,
                cause,
                restarts_in_window: self.abnormal_restart_times.len() as u64,
                window_millis: millis_u64(self.config.restart_window),
            });
        }
        self.abnormal_restart_times.push_back(now);
        Ok(())
    }

    fn observed_generation(&self) -> u64 {
        self.worker_generation_base
            .saturating_add(self.handle.lifecycle_snapshot().worker_generation)
    }

    fn runtime_facts(
        &self,
        meta: &JobMeta,
        generation_before: u64,
        retry_count: u32,
        queue_wait_millis: u64,
        call_elapsed_millis: u64,
        call_cpu_millis: Option<u64>,
        call_restart: Option<RuntimeRestartEvent>,
    ) -> RuntimeFacts {
        let generation = self.observed_generation();
        let snapshot = self.handle.lifecycle_snapshot();
        RuntimeFacts {
            native_compatibility: None,
            cli_file_verification: None,
            worker_generation: generation,
            worker_restarted: call_restart.is_some() || generation > generation_before,
            retry_count,
            final_attempt: FinalAttemptStatus::Unknown,
            previous_attempt_failure: None,
            queue_wait_millis,
            call_elapsed_millis,
            call_cpu_millis,
            call_restart,
            last_restart: self.last_restart.clone(),
            rss_kib: snapshot.last_rss_kib.or(self.last_rss_kib),
            worker_lanes: 1,
            import_profile: Some(meta.import_fingerprint.clone()),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restart_stats.total,
            restarts_by_cause: self.restart_stats.by_cause.clone(),
            import_residue_bytes: Some(snapshot.import_residue_bytes),
            import_residue_limit_bytes: snapshot.import_residue_limit_bytes,
        }
    }

    fn publish_runtime(&self, runtime: &RuntimeFacts) {
        *self.runtime.lock() = RuntimeSnapshot {
            worker_generation: runtime.worker_generation,
            last_restart: runtime.last_restart.clone().or_else(|| runtime.call_restart.clone()),
            rss_kib: runtime.rss_kib,
            import_profile: runtime.import_profile.clone(),
            profile_switch_count: runtime.profile_switch_count,
            restarts_total: runtime.restarts_total,
            restarts_by_cause: runtime.restarts_by_cause.clone(),
            import_residue_bytes: runtime.import_residue_bytes,
            import_residue_limit_bytes: runtime.import_residue_limit_bytes,
        };
    }

    fn worker_unavailable_for(
        &self,
        meta: &JobMeta,
        reason: String,
        retryable: bool,
        worker_restarted: bool,
        cause: Option<RestartCause>,
        limit_kib: Option<u64>,
        retry_after_millis: Option<u64>,
    ) -> ServerError {
        let snapshot = self.runtime.lock().facts();
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            worker_stop_receipt: None,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: meta.imports.clone(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
            blocked_by: None,
            restart_cause: cause.map(|cause| cause.as_str().to_owned()),
            rss_kib: self.last_rss_kib,
            limit_kib,
            retry_after_millis,
            restarts_in_window: Some(self.abnormal_restart_times.len() as u64),
            window_millis: Some(millis_u64(self.config.restart_window)),
            runtime: snapshot,
            reason,
            toolchain_advisories: self.config.toolchain_advisories.clone(),
        })
    }

    fn shutdown_unavailable(&self, meta: &JobMeta, reason: &'static str) -> ServerError {
        self.worker_unavailable_for(
            meta,
            reason.to_owned(),
            reason != "request_cancelled",
            false,
            None,
            None,
            None,
        )
    }

    fn restart_limit_error(&self, imports: &[String], limit: RestartLimitExceeded) -> ServerError {
        let snapshot = self.runtime.lock().facts();
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable: false,
            worker_restarted: false,
            worker_stop_receipt: None,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
            reason: limit.message,
            blocked_by: None,
            restart_cause: Some(limit.cause.as_str().to_owned()),
            rss_kib: self.last_rss_kib,
            limit_kib: None,
            retry_after_millis: Some(limit.window_millis),
            restarts_in_window: Some(limit.restarts_in_window),
            window_millis: Some(limit.window_millis),
            runtime: snapshot,
            toolchain_advisories: self.config.toolchain_advisories.clone(),
        })
    }
}

/// Project one exact upstream request receipt; never infer one from counters
/// or a typed error whose original stop may precede a failed replacement.
fn confirmed_stop_receipt(
    session_id: &str,
    operation_id: uuid::Uuid,
    scope: &str,
    generation_base: u64,
    receipt: &LeanWorkerRequestStopReceipt,
    cause: &str,
) -> WorkerStopReceipt {
    WorkerStopReceipt {
        operation_id: operation_id.to_string(),
        worker_session_id: session_id.to_owned(),
        worker_scope: scope.to_owned(),
        worker_generation: generation_base.saturating_add(receipt.worker_generation()),
        worker_request_id: Some(receipt.request_id()),
        worker_pid: Some(receipt.process_id()),
        operation: receipt.operation().to_owned(),
        cause: cause.to_owned(),
        exit_success: Some(receipt.exit().success),
        exit_code: receipt.exit().code,
        exit_status: Some(receipt.exit().status.clone()),
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "receipt identity is authoritative; other worker errors share a diagnostic cause"
)]
fn stop_cause(error: &LeanWorkerError, after: &LeanWorkerLifecycleSnapshot) -> &'static str {
    match error {
        LeanWorkerError::Timeout { .. } => "timeout",
        LeanWorkerError::Cancelled { .. } => "cancelled",
        LeanWorkerError::ChildPanicOrAbort { .. } => "child_abort",
        LeanWorkerError::ChildExited { .. } => "child_exit",
        _ => after
            .last_restart_reason
            .as_ref()
            .map_or("worker_stopped", |reason| restart_cause_from_worker(reason).as_str()),
    }
}

fn with_stop_receipt(mut error: ServerError, receipt: Option<WorkerStopReceipt>) -> ServerError {
    if let ServerError::WorkerUnavailable(failure) = &mut error {
        failure.worker_stop_receipt = receipt;
    }
    error
}

struct RestartLimitExceeded {
    message: String,
    cause: RestartCause,
    restarts_in_window: u64,
    window_millis: u64,
}

fn actor_main(
    config: ActorConfig,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>>,
    initialization: CallOperation,
) {
    let (handle, runtime_toolchain) = match open_worker(&config) {
        Ok(value) => {
            initialization.returned();
            value
        }
        Err(err) => {
            // open_worker has returned and this actor is about to exit, so no
            // later spawn can invalidate zero on this exact configuration.
            if config.process_tracker.active_processes() == 0 {
                initialization.returned();
            } else {
                let tracker = config.process_tracker.clone();
                initialization.defer_scope_closed(Arc::new(move || {
                    if tracker.active_processes() == 0 {
                        crate::cancellation::DeferredStopState::ScopeClosedWithoutStatus
                    } else {
                        crate::cancellation::DeferredStopState::Pending
                    }
                }));
            }
            let _ = init_reply.send(Err(err));
            return;
        }
    };
    let mut state = ProjectActorState {
        config: config.clone(),
        handle,
        worker_generation_base: 1,
        last_restart: None,
        imports_seen: Vec::new(),
        loaded_at: None,
        profile_switch_count: 0,
        last_rss_kib: None,
        runtime: Arc::clone(&config.runtime),
        abnormal_restart_times: VecDeque::new(),
        restart_stats: RestartStats::default(),
        cycled_while_idle: false,
    };

    let (tx, mut rx) = mpsc::channel::<ProjectMessage>(config.mailbox_capacity);
    state.poll_worker_liveness();
    // If the opener disappeared, dropping the returned sender closes rx and
    // still takes the same acknowledged worker shutdown path below.
    let _ = init_reply.send(Ok((runtime_toolchain, tx)));

    // Every reply is sent inside `handle_message` before it returns, so by the
    // time control reaches the bottom of this loop the client already has its
    // answer. That is what makes an idle cycle free: it happens after the work
    // it would otherwise have delayed.
    let mut idle_since = Instant::now();
    loop {
        let message = match config.tokio_handle.as_ref() {
            None => rx.blocking_recv(),
            Some(handle) => {
                match handle.block_on(async { tokio::time::timeout(WORKER_LIVENESS_POLL_INTERVAL, rx.recv()).await }) {
                    Ok(message) => message,
                    Err(_elapsed) => {
                        state.poll_worker_liveness();
                        if !config.healthy.load(Ordering::Acquire) {
                            rx.close();
                            continue;
                        }
                        if state.idle_grace().is_some_and(|grace| idle_since.elapsed() >= grace) {
                            state.cycle_on_quiescence();
                        }
                        continue;
                    }
                }
            }
        };
        let Some(message) = message else { break };
        idle_since = Instant::now();
        // Even a full mailbox must not starve owned process observation.
        state.poll_worker_liveness();
        if !config.healthy.load(Ordering::Acquire) {
            message.reject(&state, "project_shutting_down");
            rx.close();
            continue;
        }
        if message.meta().cancellation.is_cancelled() {
            message.reject(&state, "request_cancelled");
            continue;
        }
        if !message.meta().queue.start() {
            message.reject(&state, "queue_wait_timeout");
            continue;
        }
        state.handle_message(message);
        idle_since = Instant::now();
        if config.startup_uncertain.load(Ordering::Acquire) || !config.healthy.load(Ordering::Acquire) {
            config.healthy.store(false, Ordering::Release);
            rx.close();
            continue;
        }
        // Only where `idle_grace` can never arm. With a runtime available the
        // quiescence tick above does this job two seconds later and pre-warms as
        // well, so cycling here would spend the generation on the strictly worse
        // of the two paths.
        if config.tokio_handle.is_none() && rx.is_empty() {
            state.cycle_after_reply();
        }
    }

    match state.handle.shutdown() {
        Ok(report) => {
            tracing::debug!(
                outcome = ?report.outcome,
                elapsed_millis = millis_u64(report.elapsed),
                wait_millis = millis_u64(report.wait_elapsed),
                "project worker shutdown complete"
            );
        }
        Err(err) => {
            tracing::error!(error = %err, "project worker shutdown uncertain; retaining worker capacity");
        }
    }
}

fn open_worker(config: &ActorConfig) -> Result<(LeanWorkerHostHandle, String)> {
    // `open` performs static validation and retains the one resulting worker.
    // `check` would spawn a second preflight child and discard its shutdown
    // result, which cannot establish safe capacity accounting.
    let builder = worker_builder(config)?;
    // No `RssHardLimitExceeded` arm: this server no longer sets
    // `rss_hard_limit`, so the supervisor cannot produce one. See
    // `lean_max_memory_kib_for`.
    let handle = builder.open().map_err(|error| {
        let unresolved = config.process_tracker.active_processes() != 0;
        config.startup_uncertain.store(unresolved, Ordering::Release);
        if unresolved {
            config.healthy.store(false, Ordering::Release);
            tracing::error!(error = %error, "worker startup has an unresolved owned scope; retaining capacity");
        }
        map_worker_err(error)
    })?;
    let runtime_toolchain = handle
        .runtime_metadata()
        .lean_version
        .unwrap_or_else(|| config.toolchain_label.clone());
    Ok((handle, runtime_toolchain))
}

/// One policy restart, on the one quantity that accumulates without bound: the
/// bytes an import retains and the child cannot give back.
///
/// `disabled()`'s own doc warns that a long-running host wants
/// `memory_bounded`, "because fresh imports retain Lean process-global state
/// until the child exits" — session reuse made that true per *import* rather
/// than per *call*, and `runtime_config::WORKER_IMPORT_RESIDUE_CEILING_BYTES`
/// explains why the bound is denominated in the bytes those imports retain
/// rather than in their number. [`WORKER_MAX_IMPORTS_BACKSTOP`] rides behind it
/// as the guard against a child that reports no residue at all.
///
/// Still no `max_rss_kib`, no `max_requests`, no `rss_hard_limit`. The Lean heap
/// is backstopped above this by [`ProjectRuntimeConfig::lean_max_memory_kib`];
/// the crash-loop breaker ([`ProjectRuntimeConfig::max_restarts_per_window`])
/// and the request deadline are this actor's own.
fn residue_restart_policy(config: &ActorConfig) -> LeanWorkerRestartPolicy {
    LeanWorkerRestartPolicy::default()
        .max_import_residue_bytes(config.import_residue_budget_bytes)
        .max_imports(WORKER_MAX_IMPORTS_BACKSTOP)
        .max_restarts_per_window(SUPERVISOR_RESTART_INTENSITY, RESTART_WINDOW)
}

/// The worker child of one actor generation, ordinary or companion: the
/// project's worker and toolchain with the installed shims. Prepared storage
/// facts never substitute for loaded checks.
fn native_worker_child(config: &ActorConfig, scope: &str) -> Result<LeanWorkerChild> {
    // Unit actor tests use a framed Python child that never loads Lean. The
    // production and integration-test binaries always require prepared shims.
    #[cfg(test)]
    if config.prepared_host_artifacts.is_none() {
        return Ok(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ));
    }
    let Some(artifacts) = &config.prepared_host_artifacts else {
        return Err(ServerError::BadProject(
            "native actor requires prepared host artifacts".to_owned(),
        ));
    };
    let mut child = LeanWorkerChild::for_toolchain(config.worker_path.clone(), config.lean_sysroot.clone())
        .prepared_host_artifacts(artifacts.clone());
    if scope == "main" {
        // The worker admits an import only through exactly these roots.
        let roots = crate::lake_meta::manifest_artifact_roots(&config.lake_root)
            .into_iter()
            .map(|root| {
                root.into_os_string()
                    .into_string()
                    .map_err(|_| ServerError::BadProject("project import root is not UTF-8".to_owned()))
            })
            .collect::<Result<Vec<_>>>()?;
        let input = serde_json::to_string(&roots).map_err(|error| ServerError::Internal(error.to_string()))?;
        child = child
            .application_launch_input(&input)
            .map_err(|error| ServerError::BadProject(error.to_string()))?;
    }
    Ok(child)
}

fn worker_builder(config: &ActorConfig) -> Result<LeanWorkerHostHandleBuilder> {
    let restart_policy = residue_restart_policy(config);
    let module_cache_limits = module_cache_limits();
    let contract =
        crate::broker::MaintenanceAccess::native_contract(&config.toolchain_label).map_err(ServerError::BadProject)?;
    let expectation = crate::runtime_compatibility::session_expectation(&config.toolchain_label, contract.as_ref())
        .map_err(ServerError::BadProject)?;
    Ok(
        LeanWorkerHostHandleBuilder::shims_only(&config.lake_root, std::iter::empty::<String>())
            .require_host_runtime(expectation)
            .worker_child(native_worker_child(config, "main")?)
            .startup_timeout(Duration::from_secs(30))
            .request_timeout(Duration::from_millis(config.request_timeout_millis))
            .process_tracker(config.process_tracker.clone())
            .restart_policy(restart_policy)
            .session_pool_capacity(config.session_pool_capacity)
            .lean_max_memory_kib(config.lean_max_memory_kib)
            .module_cache_limits(module_cache_limits),
    )
}

/// Worker-side module snapshot cache limits.
///
/// The RSS guard is pinned to an unreachable ceiling, i.e. off. It is not a
/// knob because no finite value behaves sensibly: the worker clears the *whole*
/// snapshot cache at the top of every module-query batch whenever its RSS is at
/// or above the guard, and RSS counts the shared, clean, reclaimable `.olean`
/// pages Lean mmaps. Those alone put any Mathlib-scale worker past both our old
/// 2 GiB setting and the worker's own 3 GiB default, so the guard fired on
/// import-set size rather than on cache growth and wiped the cache on every
/// single query — making the snapshot cache unreachable in exactly the projects
/// that need it. Cache size is bounded by the worker's entry/TTL/byte limits,
/// which are the bounds that actually track the cache.
fn module_cache_limits() -> LeanWorkerModuleCacheLimits {
    LeanWorkerModuleCacheLimits::default().rss_guard_kib(0)
}

fn actor_thread_name(canonical_root: &Path) -> String {
    let basename = canonical_root.file_name().and_then(|s| s.to_str()).unwrap_or("project");
    format!("lean-host-mcp/project/{basename}")
}

fn worker_error_is_recoverable_death(err: &LeanWorkerError) -> bool {
    matches!(
        err,
        LeanWorkerError::ChildExited { .. } | LeanWorkerError::ChildPanicOrAbort { .. }
    )
}

fn worker_error_is_session_missing(err: &LeanWorkerError) -> bool {
    matches!(err, LeanWorkerError::Worker { code, .. } if code == "lean_rs.worker.session_missing")
}

fn import_fingerprint(imports: &[String]) -> String {
    imports.join("\n")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "unit tests use expect/unwrap_err to state the branch under test directly"
)]
mod tests;

#[cfg(all(test, unix))]
#[path = "project/receipt_tests.rs"]
pub(crate) mod receipt_tests;
