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
    LeanWorkerCapabilityBuilder, LeanWorkerChild, LeanWorkerDeclarationInspectionRequest,
    LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationSearch, LeanWorkerDeclarationSearchResult,
    LeanWorkerDeclarationVerificationBatchRequest, LeanWorkerDeclarationVerificationBatchResult,
    LeanWorkerDeclarationVerificationRequest, LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions,
    LeanWorkerError, LeanWorkerHostHandle, LeanWorkerHostHandleBuilder, LeanWorkerLifecycleSnapshot,
    LeanWorkerModuleCacheLimits, LeanWorkerModuleQuery, LeanWorkerModuleQueryBatchOutcome,
    LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector, LeanWorkerOutputBudgets,
    LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult, LeanWorkerRestartPolicy, LeanWorkerRestartReason,
};
use lean_semantic_search_runtime::SemanticSearchRuntimeBuild;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::config_file::RuntimeFileConfig;
use crate::envelope::{Freshness, RuntimeFacts, RuntimeRestartEvent};
use crate::error::{Result, ServerError, WorkerUnavailable, map_worker_err};
use crate::lake_meta::{self, LakeProjectMeta};
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};
use crate::toolchain::{Readiness, ToolchainId, WorkerBinary};

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
/// Lean *heap* ceiling for a worker child, enforced inside the child by
/// `lean_internal_set_max_memory`.
///
/// This replaced four RSS thresholds — an import-switch soft cycle, a post-job
/// recycle, a 16 GiB in-flight hard kill, and a forced recycle every 64
/// requests. All four existed to contain a child whose RSS grew with the
/// *number of tool calls*, because every session open re-ran `importModules`
/// and those regions were never reclaimable. The worker child now reuses a
/// matching session, so the growth they contained is gone, and what is left is
/// one runaway elaboration — which is a heap problem, not an RSS one.
///
/// RSS was the wrong quantity to measure regardless. It counts shared, clean,
/// mmapped `.olean` pages that the kernel can drop at will: a Mathlib child
/// reads multiple GiB of RSS while its actual footprint is a fraction of that,
/// so an RSS ceiling fires on healthy workers and says nothing about the
/// unhealthy ones. Lean's own heap accounting is the quantity that
/// distinguishes them.
///
/// Overrun surfaces as a `LeanWorkerElabFailure` inside the `ok` payload —
/// the Lean-domain-failure contract — rather than as a kill, a result taint, or
/// a restart. 8 GiB because a single elaboration that legitimately needs more
/// than that is a Lean-side problem an agent cannot act on either way.
const LEAN_MAX_MEMORY_KIB: u64 = 8 * 1024 * 1024;
/// Depth of one project's job queue, and the only admission mechanism in the
/// server. The actor thread runs one job at a time, so this bounds how many
/// callers may be waiting on a single worker before the project sheds load
/// retryably rather than queueing without limit. Sized to the process-wide
/// waiter bound the deleted semantic-admission semaphore used to enforce, now
/// applied per project — different projects no longer contend for one budget.
const PROJECT_MAILBOX_CAPACITY: usize = 16;
/// Per-request worker deadline. Covers one tool call end to end (live rows,
/// diagnostics, terminal response); on expiry the worker is recycled and the
/// call returns a retryable runtime error. Replaces the worker-parent's 10-min
/// `long_running_requests` profile, which let whole-project scans (e.g.
/// `find_references` at project scope) appear to hang. Raise it for unusually
/// heavy modules whose `verify`/`proof_state` legitimately runs longer.
const REQUEST_TIMEOUT_MILLIS: u64 = 120 * 1000;
/// How many *imports* one worker child may run before the supervisor cycles it,
/// and — the same number, structurally — how many imported environments that
/// child pools.
///
/// This is the **entire** memory policy for import residue.
/// [`LEAN_MAX_MEMORY_KIB`] bounds the Lean *heap*; an import's compacted regions
/// are mmapped outside that heap, so it does not see them. Nothing else does
/// either.
///
/// Session reuse removed growth with call count, but not with import count: a
/// Lean environment imported with `loadExts := true` cannot be reclaimed
/// (`Environment.freeRegions` is unsound there), so every import a child
/// performs is retained for the life of that child even after its session is
/// dropped. Measured on `fixtures/lean` by alternating two import profiles
/// against one server (`scripts/memory_stability.py`): child RSS rose from
/// 1.26 GiB to 11.2 GiB over ~10 switches, per-call latency degraded from 0.4 s
/// to 5 s, and the OS killed the child with `SIGKILL` at 178 calls.
///
/// So the bound belongs on imports. A workload that repeats one import profile
/// — the proof loop this server exists to serve — never trips it, because a
/// reused session is not an import and does not count
/// (`Response::HostSessionReused` leaves `imports_since_restart` untouched).
///
/// Since the child pools sessions rather than dropping the outgoing one, a
/// workload that *switches* profiles doesn't trip it either, as long as it
/// cycles among at most this many: returning to a pooled profile is a key
/// compare, not an import. That makes this a bound on **distinct** profiles per
/// child generation, which is why it can afford to be larger than the `2` that
/// bounded switches.
///
/// `4` from the three-arm measurement in `lean-rs`'s `long_session_memory`
/// example (`pooled-distinct`), fresh process per arm, `.olean`-backed RSS at
/// the end:
///
/// | distinct | imports | live envs | peak RSS |
/// |----------|---------|-----------|----------|
/// | 2, dropped   | 2  | 1 | 1.650 GB |
/// | 2, pooled    | 2  | 2 | 1.679 GB |
/// | 2, alternating | 8 | 1 | 8.824 GB |
/// | 4, dropped   | 4  | 1 | 4.043 GB |
/// | 4, pooled    | 4  | 4 | 4.190 GB |
/// | 4, alternating | 16 | 1 | ~7.9 GB |
///
/// Holding an environment alive instead of dropping it costs 30–50 MB against
/// the 0.8–1.0 GiB the import that produced it costs — 4–6%, because the
/// regions outlive the environment either way. So the pool is nearly free and
/// the *imports* are what to count: four distinct profiles cost ~4.2 GiB
/// pooled versus ~7.9 GiB and four times the imports without.
///
/// This is deliberately *not* an RSS threshold. RSS counts shared, clean,
/// mmapped `.olean` pages, so any byte-valued limit either fires immediately on
/// a Mathlib-scale project or never fires on a small one; the import count is
/// the quantity that actually leaks, and reading it costs no `ps` fork.
const WORKER_MAX_IMPORTS: u64 = 4;
const MAX_JOB_RETRIES: u32 = 1;
const MAX_RESTARTS_PER_WINDOW: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_mins(1);
/// Backstop on *every* worker cycle in one [`RESTART_WINDOW`], planned or not.
///
/// The supervisor's own limit, distinct from [`MAX_RESTARTS_PER_WINDOW`], which
/// this actor applies to abnormal causes only. The supervisor cannot make that
/// distinction, so its default of 16 counts [`WORKER_MAX_IMPORTS`] cycles —
/// and exhausting it is terminal: the supervisor refuses to spawn a
/// replacement and every later call fails with "shutdown is in progress". A
/// workload that alternates import profiles reached that state after 16 calls
/// and never recovered. Sized far above any legitimate cycle rate so that only
/// a genuine spawn loop can reach it; the discriminating breaker is this
/// actor's own.
const SUPERVISOR_RESTART_INTENSITY: u64 = 256;

/// Runtime policy for one private project actor.
///
/// The binary parses this once at server startup and passes it into the
/// broker. Tests and embedders can construct the default directly without
/// rereading process environment during project open.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectRuntimeConfig {
    lean_max_memory_kib: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
}

impl Default for ProjectRuntimeConfig {
    fn default() -> Self {
        Self {
            lean_max_memory_kib: LEAN_MAX_MEMORY_KIB,
            request_timeout_millis: REQUEST_TIMEOUT_MILLIS,
            mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
            max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
            restart_window: RESTART_WINDOW,
        }
    }
}

impl ProjectRuntimeConfig {
    /// Parse runtime env vars once at server startup.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when a runtime env var is malformed, zero
    /// where zero is unsafe.
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_file(&RuntimeFileConfig::default())
    }

    /// Resolve the runtime policy with a config-file section as the layer
    /// beneath env vars: each knob is `env var > file > built-in default`.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is malformed, or a resolved
    /// value (from env or file) is zero where zero is unsafe.
    pub fn from_env_with_file(file: &RuntimeFileConfig) -> Result<Self> {
        parse_runtime_config(
            RuntimeEnv {
                lean_max_memory_kib: runtime_env_var("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB")?,
                request_timeout_millis: runtime_env_var("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS")?,
                project_mailbox_capacity: runtime_env_var("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY")?,
                worker_restart_limit: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_LIMIT")?,
                worker_restart_window_secs: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS")?,
            },
            file,
        )
    }

    /// The Lean heap ceiling applied to each worker child; see
    /// [`LEAN_MAX_MEMORY_KIB`].
    #[must_use]
    pub const fn lean_max_memory_kib(&self) -> u64 {
        self.lean_max_memory_kib
    }

    #[must_use]
    pub const fn request_timeout_millis(&self) -> u64 {
        self.request_timeout_millis
    }

    #[must_use]
    pub const fn mailbox_capacity(&self) -> usize {
        self.mailbox_capacity
    }

    #[must_use]
    pub const fn max_restarts_per_window(&self) -> usize {
        self.max_restarts_per_window
    }

    #[must_use]
    pub const fn restart_window(&self) -> Duration {
        self.restart_window
    }
}

#[derive(Debug, Default)]
struct RuntimeEnv {
    lean_max_memory_kib: Option<String>,
    request_timeout_millis: Option<String>,
    project_mailbox_capacity: Option<String>,
    worker_restart_limit: Option<String>,
    worker_restart_window_secs: Option<String>,
}

fn parse_runtime_config(env: RuntimeEnv, file: &RuntimeFileConfig) -> Result<ProjectRuntimeConfig> {
    let defaults = ProjectRuntimeConfig::default();
    let config = ProjectRuntimeConfig {
        lean_max_memory_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB",
            env.lean_max_memory_kib.as_deref(),
            file.lean_max_memory_kib,
            defaults.lean_max_memory_kib,
        )?,
        request_timeout_millis: parse_nonzero_u64(
            "LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS",
            env.request_timeout_millis.as_deref(),
            file.request_timeout_millis,
            defaults.request_timeout_millis,
        )?,
        mailbox_capacity: parse_nonzero_usize(
            "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
            env.project_mailbox_capacity.as_deref(),
            file.project_mailbox_capacity,
            defaults.mailbox_capacity,
        )?,
        max_restarts_per_window: parse_nonzero_usize(
            "LEAN_HOST_MCP_WORKER_RESTART_LIMIT",
            env.worker_restart_limit.as_deref(),
            file.worker_restart_limit,
            defaults.max_restarts_per_window,
        )?,
        restart_window: Duration::from_secs(parse_nonzero_u64(
            "LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS",
            env.worker_restart_window_secs.as_deref(),
            file.worker_restart_window_secs,
            defaults.restart_window.as_secs(),
        )?),
    };
    Ok(config)
}

fn runtime_env_var(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err @ std::env::VarError::NotUnicode(_)) => {
            Err(ServerError::Internal(format!("{name} is not valid unicode: {err}")))
        }
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryPolicy {
    RetryOnceReadOnly,
}

impl RetryPolicy {
    fn retries(self) -> u32 {
        match self {
            Self::RetryOnceReadOnly => MAX_JOB_RETRIES,
        }
    }
}

struct ActiveJobGuard {
    active_jobs: Arc<AtomicUsize>,
}

impl Drop for ActiveJobGuard {
    fn drop(&mut self) {
        self.active_jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

struct JobMeta {
    imports: Vec<String>,
    import_fingerprint: String,
    _created_at: Instant,
    queued_at: Instant,
    _correlation_id: uuid::Uuid,
    retry_policy: RetryPolicy,
    _active_job: ActiveJobGuard,
}

enum ProjectMessage {
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
    SemanticProofSearch {
        meta: JobMeta,
        request: SemanticProofSearchRequest,
        reply: oneshot::Sender<Result<ProjectCall<SemanticProofSearchResult>>>,
    },
}

impl ProjectMessage {
    fn imports(&self) -> &[String] {
        match self {
            Self::ModuleQuery { meta, .. }
            | Self::ModuleQueryBatch { meta, .. }
            | Self::DeclarationInspection { meta, .. }
            | Self::DeclarationSearch { meta, .. }
            | Self::ProofAttempt { meta, .. }
            | Self::DeclarationVerification { meta, .. }
            | Self::DeclarationVerificationBatch { meta, .. }
            | Self::SemanticProofSearch { meta, .. } => &meta.imports,
        }
    }

    fn reject(self, state: &ProjectActorState, reason: &'static str) {
        match self {
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
            Self::SemanticProofSearch { meta, reply, .. } => {
                let _ = reply.send(Err(state.shutdown_unavailable(&meta, reason)));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestartCause {
    /// A `.olean` among the live session's imports was rebuilt on disk. The
    /// session's environment is a snapshot taken at import, so it must be
    /// dropped or the call answers from a stale Lean environment.
    ArtifactsRebuilt,
    RssPostJob,
    RssHardLimit,
    MaxRequests,
    MaxImports,
    Idle,
    Timeout,
    Cancelled,
    ChildExit,
    ChildAbort,
    SessionMissing,
    Explicit,
    WorkerInternal,
}

impl RestartCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ArtifactsRebuilt => "artifacts_rebuilt",
            Self::RssPostJob => "rss_post_job",
            Self::RssHardLimit => "rss_hard_limit_exceeded",
            Self::MaxRequests => "max_requests",
            Self::MaxImports => "max_imports",
            Self::Idle => "idle",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ChildExit => "child_exit",
            Self::ChildAbort => "child_abort",
            Self::SessionMissing => "session_missing",
            Self::Explicit => "explicit",
            Self::WorkerInternal => "worker_internal",
        }
    }

    const fn counts_toward_restart_limit(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::Cancelled
                | Self::ChildExit
                | Self::ChildAbort
                | Self::SessionMissing
                | Self::RssHardLimit
        )
    }

    const fn is_planned(self) -> bool {
        !self.counts_toward_restart_limit()
    }
}

fn restart_event(
    cause: RestartCause,
    reason: impl Into<String>,
    worker_generation: u64,
    rss_kib: Option<u64>,
    limit_kib: Option<u64>,
) -> RuntimeRestartEvent {
    RuntimeRestartEvent {
        cause: cause.as_str().to_owned(),
        reason: reason.into(),
        worker_generation,
        planned: cause.is_planned(),
        rss_kib,
        limit_kib,
    }
}

/// Per-project recycle tally over the worker's lifetime, all causes.
///
/// Recorded once per event at [`ProjectActorState::record_restart`] and copied
/// into [`RuntimeSnapshot`] on publish, so the no-call and error paths report
/// the same totals a live call would. This answers "how *often*, and why?"; the
/// single most-recent event stays in `last_restart`.
#[derive(Debug, Clone, Default)]
struct RestartStats {
    total: u64,
    by_cause: BTreeMap<String, u64>,
}

impl RestartStats {
    fn observe(&mut self, cause: &str) {
        self.total = self.total.saturating_add(1);
        let count = self.by_cause.entry(cause.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

/// Emit one structured log line for a recycle. Level tracks the *signal*, not
/// `planned`: crash/abnormal causes `warn`, memory-pressure cycles `info` (the
/// frequency an operator tuning the RSS budget watches), pure hygiene `debug`.
fn log_restart(event: &RuntimeRestartEvent, restarts_total: u64) {
    macro_rules! emit {
        ($level:ident, $msg:literal) => {
            tracing::$level!(
                cause = %event.cause,
                reason = %event.reason,
                worker_generation = event.worker_generation,
                rss_kib = ?event.rss_kib,
                limit_kib = ?event.limit_kib,
                planned = event.planned,
                restarts_total,
                $msg
            )
        };
    }
    match event.cause.as_str() {
        "rss_hard_limit_exceeded"
        | "child_abort"
        | "child_exit"
        | "session_missing"
        | "worker_internal"
        | "timeout"
        | "cancelled" => emit!(warn, "worker recycled (abnormal)"),
        "rss_post_job" => emit!(info, "worker recycled (memory pressure)"),
        "artifacts_rebuilt" => emit!(info, "worker recycled (imports rebuilt on disk)"),
        _ => emit!(debug, "worker recycled (hygiene)"),
    }
}

fn restart_cause_from_worker(reason: &LeanWorkerRestartReason) -> RestartCause {
    match reason.stable_cause() {
        "explicit" => RestartCause::Explicit,
        "max_requests" => RestartCause::MaxRequests,
        "max_imports" => RestartCause::MaxImports,
        "rss_ceiling" => RestartCause::RssPostJob,
        "rss_hard_limit" => RestartCause::RssHardLimit,
        "idle" => RestartCause::Idle,
        "cancelled" => RestartCause::Cancelled,
        "timeout" => RestartCause::Timeout,
        _ => RestartCause::WorkerInternal,
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
}

impl RuntimeSnapshot {
    fn facts(&self) -> RuntimeFacts {
        RuntimeFacts {
            worker_generation: self.worker_generation,
            worker_restarted: false,
            retry_count: 0,
            queue_wait_millis: 0,
            call_restart: None,
            last_restart: self.last_restart.clone(),
            rss_kib: self.rss_kib,
            worker_lanes: 1,
            import_profile: self.import_profile.clone(),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restarts_total,
            restarts_by_cause: self.restarts_by_cause.clone(),
        }
    }
}

/// One Lake project, one serialized worker controller, one in-memory cache.
/// Cheap to clone via `Arc`.
pub(crate) struct LeanProject {
    canonical_root: PathBuf,
    toolchain: String,
    package: Option<String>,
    library: Option<String>,
    manifest_hash: String,
    session_id: String,
    /// Toolchain-provenance advisories captured at open (unknown pin, missing
    /// sidecar). Surfaced into every response's envelope warnings via
    /// [`Self::freshness`]; empty for a fully-vouched-for worker.
    open_warnings: Vec<String>,
    actor_tx: Mutex<Option<mpsc::Sender<ProjectMessage>>>,
    active_jobs: Arc<AtomicUsize>,
    healthy: Arc<AtomicBool>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    module_queries: ModuleQueryCache,
}

impl std::fmt::Debug for LeanProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeanProject")
            .field("canonical_root", &self.canonical_root)
            .field("toolchain", &self.toolchain)
            .field("package", &self.package)
            .field("library", &self.library)
            .field("manifest_hash", &self.manifest_hash)
            .finish_non_exhaustive()
    }
}

impl LeanProject {
    pub(crate) fn open(meta: LakeProjectMeta, runtime_config: ProjectRuntimeConfig) -> Result<Arc<Self>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
        }));
        let active_jobs = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicBool::new(true));
        let (config, open_warnings) = ActorConfig::from_meta(
            &meta,
            session_id.clone(),
            Arc::clone(&runtime),
            Arc::clone(&healthy),
            runtime_config,
        )?;
        type InitMsg = std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let thread_name = actor_thread_name(&meta.canonical_root);

        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                actor_main(config, init_tx);
            })
            .map_err(|e| ServerError::Internal(format!("spawn project actor thread: {e}")))?;

        let (runtime_toolchain, actor_tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("project actor thread died during init".into()))??;

        let cache_cap = NonZeroUsize::new(MODULE_QUERY_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        Ok(Arc::new(Self {
            canonical_root: meta.canonical_root,
            toolchain: runtime_toolchain,
            package: meta.package,
            library: meta.library,
            manifest_hash: meta.manifest_hash,
            session_id,
            open_warnings,
            actor_tx: Mutex::new(Some(actor_tx)),
            active_jobs,
            healthy,
            runtime,
            module_queries: ModuleQueryCache::with_capacity(cache_cap),
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
    ) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQuery {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
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
    ) -> Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQueryBatch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
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
    ) -> Result<ProjectCall<LeanWorkerDeclarationInspectionResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationInspection {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
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
    ) -> Result<ProjectCall<Vec<LeanWorkerDeclarationSearchResult>>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationSearch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            requests,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Run source-backed semantic proof search through this project's actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when semantic capability setup, mailbox enqueue,
    /// actor reply, or worker execution fails.
    pub(crate) async fn semantic_proof_search(
        &self,
        imports: Vec<String>,
        request: SemanticProofSearchRequest,
    ) -> Result<ProjectCall<SemanticProofSearchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::SemanticProofSearch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
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
    ) -> Result<ProjectCall<LeanWorkerProofAttemptResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ProofAttempt {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
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
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerification {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
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
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerificationBatch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    fn job_meta(&self, imports: Vec<String>, retry_policy: RetryPolicy) -> JobMeta {
        let created_at = Instant::now();
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        JobMeta {
            import_fingerprint: import_fingerprint(&imports),
            imports,
            _created_at: created_at,
            queued_at: Instant::now(),
            _correlation_id: uuid::Uuid::new_v4(),
            retry_policy,
            _active_job: ActiveJobGuard {
                active_jobs: Arc::clone(&self.active_jobs),
            },
        }
    }

    async fn enqueue<T>(
        &self,
        message: ProjectMessage,
        reply_rx: oneshot::Receiver<Result<ProjectCall<T>>>,
    ) -> Result<ProjectCall<T>>
    where
        T: Send + 'static,
    {
        let project_info = self.worker_error_context(message.imports());
        let tx = self
            .actor_tx
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| self.unavailable("project actor is stopped", false, false))?;
        match tx.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(ServerError::worker_unavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "mailbox_full".to_owned(),
                    ..project_info
                }));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.shutdown();
                return Err(ServerError::worker_unavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "mailbox_closed".to_owned(),
                    ..project_info
                }));
            }
        }

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.shutdown();
                Err(self.unavailable("mailbox_closed_before_reply", true, false))
            }
        }
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

    pub(crate) fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.actor_tx.lock().as_ref().is_some_and(|tx| !tx.is_closed())
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.active_jobs.load(Ordering::Acquire) == 0
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
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
            worker_generation: snapshot.worker_generation,
            reason: String::new(),
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
    lake_root: PathBuf,
    manifest_hash: String,
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    session_id: String,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    healthy: Arc<AtomicBool>,
    /// Where a module this project imports could have its `.olean` built.
    /// Resolved once at open: the package set is a function of
    /// `lake-manifest.json`, and a manifest change evicts the project outright.
    artifact_roots: Vec<PathBuf>,
    lean_max_memory_kib: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
    /// Open-time toolchain advisories (unknown pin, missing sidecar, no smoke
    /// record). The actor carries them so a `runtime_unavailable` it produces
    /// after worker death still flags a suspect worker. Mirrors
    /// [`LeanProject::open_warnings`]; both come from the one
    /// [`WorkerBinary::resolve_ready_for`] verdict at open.
    toolchain_advisories: Vec<String>,
}

impl ActorConfig {
    /// Resolve the pinned toolchain into a spawnable config plus any
    /// open-time provenance advisories. All version-drift situations collapse
    /// into the one [`WorkerBinary::resolve_ready_for`] verdict: hard failures
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
        let toolchain_id = ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let (worker_path, lean_sysroot, open_warnings) = match WorkerBinary::resolve_ready_for(&toolchain_id) {
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
            Readiness::Stale { toolchain, install_cmd } => {
                return Err(ServerError::BadProject(format!(
                    "worker for {toolchain} was built against a different lean.h than the toolchain now \
                     provides (header drift); rebuild it: {install_cmd}"
                )));
            }
            Readiness::Unusable {
                toolchain,
                detail,
                install_cmd,
            } => {
                return Err(ServerError::BadProject(format!(
                    "worker for {toolchain} failed its runtime smoke test ({detail}); the toolchain's \
                     libleanshared is ABI-incompatible with this lean-rs build and cannot be served. \
                     Pin a supported toolchain the host can run, or rebuild lean-rs and reinstall: {install_cmd}"
                )));
            }
            Readiness::NotInstalled { toolchain, install_cmd } => {
                return Err(ServerError::BadProject(format!(
                    "no worker binary for toolchain {toolchain}; run: {install_cmd}"
                )));
            }
            Readiness::ToolchainNotInstalled { toolchain, elan_dir } => {
                return Err(ServerError::BadProject(format!(
                    "elan toolchain {toolchain} is not installed (expected {})",
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
        let config = Self {
            lake_root: meta.canonical_root.clone(),
            manifest_hash: meta.manifest_hash.clone(),
            toolchain_label: meta.toolchain.clone(),
            worker_path,
            lean_sysroot,
            session_id,
            runtime,
            healthy,
            artifact_roots: lake_meta::artifact_roots(&meta.canonical_root),
            lean_max_memory_kib: runtime_config.lean_max_memory_kib(),
            request_timeout_millis: runtime_config.request_timeout_millis(),
            mailbox_capacity: runtime_config.mailbox_capacity(),
            max_restarts_per_window: runtime_config.max_restarts_per_window(),
            restart_window: runtime_config.restart_window(),
            toolchain_advisories: open_warnings.clone(),
        };
        Ok((config, open_warnings))
    }
}

/// One import profile the child may be holding, and the build state it was
/// holding it at.
struct ImportProfileStamp {
    fingerprint: String,
    /// Newest `.olean` mtime among that profile's imports, sampled when it was
    /// last served. `None` for an unbuilt project, where there is no mtime to
    /// compare against.
    artifact_stamp: Option<std::time::SystemTime>,
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    worker_generation_base: u64,
    last_restart: Option<RuntimeRestartEvent>,
    /// The import profiles the live worker child may still be holding, MRU at
    /// the back, bounded by [`WORKER_MAX_IMPORTS`] so it mirrors the child's own
    /// session pool.
    ///
    /// A *list* because the child pools sessions: it holds several imported
    /// environments at once, so "the profile the live session was opened with"
    /// is no longer a single value, and a stamp recorded for one profile stays
    /// relevant while another is served.
    imports_seen: Vec<ImportProfileStamp>,
    profile_switch_count: u64,
    last_rss_kib: Option<u64>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    abnormal_restart_times: VecDeque<Instant>,
    restart_stats: RestartStats,
}

impl ProjectActorState {
    fn handle_message(&mut self, message: ProjectMessage) {
        match message {
            ProjectMessage::ModuleQuery {
                meta,
                source,
                query,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.process_module_query_with_imports(imports, &source, &query, &options, None, None)
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
                let result = self.run_job(meta, |handle, imports| {
                    handle.process_module_query_batch_with_imports(
                        imports, &source, &selectors, &budgets, &options, None, None,
                    )
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationInspection { meta, request, reply } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.inspect_declaration_with_imports(imports, &request, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationSearch { meta, requests, reply } => {
                // One session for the whole group. `search_declarations_with_imports`
                // opens and drops a session per call, and every open re-imports,
                // so the per-search cost here is a worker round trip rather than
                // an import. The session borrows `&mut handle` and dies inside
                // the closure, so nothing escapes the actor's stack frame.
                let result = self.run_job(meta, |handle, imports| {
                    let mut session = handle.open_session_with_imports(imports, None, None)?;
                    requests
                        .iter()
                        .map(|request| session.search_declarations(request, None, None))
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
                let result = self.run_job(meta, |handle, imports| {
                    handle.attempt_proof_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerification {
                meta,
                request,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.verify_declaration_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::DeclarationVerificationBatch {
                meta,
                request,
                options,
                reply,
            } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.verify_declaration_batch_with_imports(imports, &request, &options, None, None)
                });
                let _ = reply.send(result);
            }
            ProjectMessage::SemanticProofSearch { meta, request, reply } => {
                let result = self.run_semantic_job(meta, &request);
                let _ = reply.send(result);
            }
        }
    }

    fn run_job<R>(
        &mut self,
        meta: JobMeta,
        job: impl Fn(&mut LeanWorkerHostHandle, Vec<String>) -> std::result::Result<R, LeanWorkerError>,
    ) -> Result<ProjectCall<R>> {
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
        let mut call_restart: Option<RuntimeRestartEvent> = self.cycle_if_imports_rebuilt(&meta)?;
        let mut lifecycle_baseline = self.handle.lifecycle_snapshot();

        let max_retries = meta.retry_policy.retries();
        let mut retry_count = 0_u32;
        loop {
            match job(&mut self.handle, meta.imports.clone()) {
                Ok(value) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    // Sampled for `RuntimeFacts.rss_kib`, which is reporting
                    // only: no threshold reads it, and nothing restarts on it.
                    self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    tracing::debug!(
                        retry_count,
                        rss_kib = ?runtime.rss_kib,
                        worker_generation = runtime.worker_generation,
                        "job complete"
                    );
                    self.publish_runtime(&runtime);
                    return Ok(ProjectCall::new(value, runtime));
                }
                Err(err) if worker_error_is_recoverable_death(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    let first_reason = err.to_string();
                    call_restart =
                        Some(self.rebuild_after_worker_death(first_reason, worker_death_cause(&err), &meta)?);
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_recoverable_death(&err) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let reason = format!("worker_died_after_retry: {err}");
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        reason,
                        true,
                        generation > generation_before,
                        Some(worker_death_cause(&err)),
                        None,
                        None,
                    ));
                }
                Err(err) if worker_error_is_session_missing(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    call_restart = Some(self.rebuild_after_worker_death(
                        format!("session_missing: {err}"),
                        RestartCause::SessionMissing,
                        &meta,
                    )?);
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_session_missing(&err) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!("session_missing: {err}"),
                        true,
                        generation > generation_before,
                        Some(RestartCause::SessionMissing),
                        None,
                        None,
                    ));
                }
                Err(LeanWorkerError::RssHardLimitExceeded {
                    operation,
                    current_kib,
                    limit_kib,
                    ..
                }) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!(
                            "rss_hard_limit_exceeded operation={operation} current_kib={current_kib} limit_kib={limit_kib}"
                        ),
                        false,
                        true,
                        Some(RestartCause::RssHardLimit),
                        Some(limit_kib),
                        None,
                    ));
                }
                Err(err) if matches!(err, LeanWorkerError::Timeout { .. }) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    let generation = self.observed_generation();
                    let runtime =
                        self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis, call_restart);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable_for(
                        &meta,
                        format!("timeout: {err}"),
                        true,
                        generation > generation_before,
                        Some(RestartCause::Timeout),
                        None,
                        None,
                    ));
                }
                Err(err) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)?;
                    return Err(map_worker_err(err));
                }
            }
        }
    }

    fn run_semantic_job(
        &self,
        meta: JobMeta,
        request: &SemanticProofSearchRequest,
    ) -> Result<ProjectCall<SemanticProofSearchResult>> {
        let _span = tracing::debug_span!(
            "semantic_job",
            session_id = %self.config.session_id,
            imports = meta.imports.len(),
            queue_wait_millis = millis_u64(meta.queued_at.elapsed()),
        )
        .entered();
        let queue_wait_millis = millis_u64(meta.queued_at.elapsed());
        let generation_before = self.observed_generation();
        let mut capability = self.open_semantic_capability(&meta)?;
        let result = {
            let mut session = capability
                .open_session_with_imports(meta.imports.clone(), None, None)
                .map_err(map_worker_err)?;
            crate::semantic_search::run_semantic_proof_search(&mut session, request)
        };
        let runtime = self.runtime_facts(&meta, generation_before, 0, queue_wait_millis, None);
        self.publish_runtime(&runtime);
        result.map(|value| ProjectCall::new(value, runtime))
    }

    /// Spawn the semantic-search child for one call. The child is dropped when
    /// the caller's `LeanWorkerCapability` goes out of scope.
    ///
    /// **Keeping it resident between calls has now been measured twice, and
    /// rejected twice for different reasons.**
    ///
    /// The first measurement — 2.71 s per-call spawn versus 3.30 s resident on
    /// `benches/search_for_proof.rs`, a 15–22% regression — was taken against a
    /// worker child that re-imported on every `open_session_with_imports`.
    /// Residency saved the process spawn while letting one child accumulate
    /// every call's unreclaimable import, and the accumulation cost more. That
    /// reason is now obsolete: a session whose imports match the live one is
    /// reused.
    ///
    /// So it was re-implemented — resident on the actor state, evicted by
    /// import fingerprint — and re-measured on the same bench: **1.35 s
    /// per-call spawn versus 1.49 s resident** (p = 0.11, confidence intervals
    /// overlapping). No significant improvement, with the point estimate again
    /// slightly worse, so the simpler per-call spawn stands. The spawn it would
    /// save is smaller than `benches/worker_cold_spawn.rs`'s 845 ms suggests:
    /// that figure is the *main* worker's cold spawn plus a first inspect, and
    /// the semantic child imports a different, smaller set.
    ///
    /// Revisit only with a workload where the semantic child's spawn is a
    /// measured majority of the call — and revert again unless the bench moves.
    fn open_semantic_capability(&self, meta: &JobMeta) -> Result<lean_rs_worker_parent::LeanWorkerCapability> {
        let runtime = semantic_runtime(&self.config.toolchain_label, &self.config.lean_sysroot).map_err(|err| {
            self.worker_unavailable_for(
                meta,
                format!("semantic runtime build failed for this toolchain: {err}"),
                true,
                false,
                None,
                None,
                None,
            )
        })?;
        semantic_capability_builder(&self.config, &runtime.built)?
            .open()
            .map_err(|err| {
                self.worker_unavailable_for(
                    meta,
                    format!(
                        "semantic capability open failed for this toolchain: {}",
                        map_worker_err(err)
                    ),
                    true,
                    false,
                    None,
                    None,
                    None,
                )
            })
    }

    /// Drop the live session when a `.olean` it imported has been rebuilt.
    ///
    /// A worker session's environment is a snapshot taken at import: the child
    /// reuses a session whose imports match, so `.olean` files written after
    /// that import are invisible to it. Before the child learned to reuse,
    /// every call re-imported, and that accident is what kept a mid-session
    /// `lake build` from being served stale answers. This restores the property
    /// deliberately, at k `stat` calls per job rather than a full import.
    ///
    /// Checked against the stamp recorded for *this* import profile, whichever
    /// profile ran in between. Until the child pooled sessions, a differing
    /// profile always re-imported, so only the immediately preceding profile
    /// could be stale and one remembered stamp sufficed. A pooled child parks
    /// the old environment instead, so a profile served three calls ago can be
    /// restored without importing — and would be served from `.olean` files
    /// rebuilt since. Recycling before the job means the job then runs on a
    /// fresh worker, which is why `artifacts_rebuilt` is not an execution taint.
    ///
    /// The stamp is read just before the session opens, not after, so a build
    /// that lands inside that window is attributed to the new session and the
    /// following call sees no advance. Closing it would cost a second stat pass
    /// per job to catch a race measured in milliseconds against a `lake build`
    /// measured in seconds; the artifact facts in `trust` still report the
    /// staleness for the modules a call actually touches.
    fn cycle_if_imports_rebuilt(&mut self, meta: &JobMeta) -> Result<Option<RuntimeRestartEvent>> {
        let stamp = lake_meta::import_artifact_stamp(&self.config.artifact_roots, &meta.imports);
        // Recorded unconditionally: this is the stamp of the artifacts the
        // session about to serve the job will have imported, whether that
        // session is a pooled one, the live one, or the replacement opened
        // below.
        let previous = self.note_import_profile(&meta.import_fingerprint, stamp);
        let (Some(previous), Some(current)) = (previous, stamp) else {
            return Ok(None);
        };
        if current <= previous {
            return Ok(None);
        }
        let reason = format!("artifacts_rebuilt imports={} newest_olean_advanced", meta.imports.len());
        self.record_restart_or_stop(RestartCause::ArtifactsRebuilt, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        self.handle.cycle().map_err(map_worker_err)?;
        self.forget_pooled_import_profiles();
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

    /// The profile the most recent job ran with, or `None` before the first job.
    ///
    /// Reported as the `import_profile` runtime fact and used to count switches.
    /// Both want "most recent", which is the back of the MRU list.
    fn current_import_profile(&self) -> Option<&str> {
        self.imports_seen.last().map(|held| held.fingerprint.as_str())
    }

    /// Record that `fingerprint` is now the most recently served profile,
    /// carrying `stamp`, and return the stamp the child last held it at.
    ///
    /// `None` covers both "the child cannot be holding this profile" and "it
    /// held it over an unbuilt project", deliberately: neither gives an mtime
    /// to compare the incoming one against, so the caller treats them alike.
    fn note_import_profile(
        &mut self,
        fingerprint: &str,
        stamp: Option<std::time::SystemTime>,
    ) -> Option<std::time::SystemTime> {
        if let Some(index) = self
            .imports_seen
            .iter()
            .position(|held| held.fingerprint == fingerprint)
        {
            let mut held = self.imports_seen.remove(index);
            let previous = held.artifact_stamp;
            held.artifact_stamp = stamp;
            self.imports_seen.push(held);
            return previous;
        }
        // Bounded by the same constant the child sizes its session pool from,
        // so the parent's picture of what the child holds cannot outgrow it.
        while self.imports_seen.len() >= usize::try_from(WORKER_MAX_IMPORTS).unwrap_or(usize::MAX) {
            self.imports_seen.remove(0);
        }
        self.imports_seen.push(ImportProfileStamp {
            fingerprint: fingerprint.to_owned(),
            artifact_stamp: stamp,
        });
        None
    }

    /// Forget every pooled profile except the one the current job is serving.
    ///
    /// Called after the child is replaced: the new child holds nothing its
    /// predecessor held, so every remembered stamp but one describes an
    /// environment that no longer exists. The exception is the current job's
    /// profile, which the caller records before the replacement and the job
    /// then re-imports into the fresh child — dropping it too would blind the
    /// *next* call to a rebuild that lands right after this one.
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
    /// import costs. What bounds the count is [`WORKER_MAX_IMPORTS`]; what
    /// bounds the heap is [`LEAN_MAX_MEMORY_KIB`]. Neither is a process-level
    /// RSS reading, which mostly measured mmapped `.olean` pages.
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
        let (handle, _) = open_worker(&self.config, false)?;
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
                worker_generation: self.observed_generation(),
                worker_restarted: false,
                retry_count: MAX_JOB_RETRIES,
                queue_wait_millis: 0,
                call_restart: None,
                last_restart: Some(event),
                rss_kib: self.last_rss_kib,
                worker_lanes: 1,
                import_profile: self.current_import_profile().map(str::to_owned),
                profile_switch_count: self.profile_switch_count,
                restarts_total: self.restart_stats.total,
                restarts_by_cause: self.restart_stats.by_cause.clone(),
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
        call_restart: Option<RuntimeRestartEvent>,
    ) -> RuntimeFacts {
        let generation = self.observed_generation();
        let snapshot = self.handle.lifecycle_snapshot();
        RuntimeFacts {
            worker_generation: generation,
            worker_restarted: call_restart.is_some() || generation > generation_before,
            retry_count,
            queue_wait_millis,
            call_restart,
            last_restart: self.last_restart.clone(),
            rss_kib: snapshot.last_rss_kib.or(self.last_rss_kib),
            worker_lanes: 1,
            import_profile: Some(meta.import_fingerprint.clone()),
            profile_switch_count: self.profile_switch_count,
            restarts_total: self.restart_stats.total,
            restarts_by_cause: self.restart_stats.by_cause.clone(),
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
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: meta.imports.clone(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
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
        self.worker_unavailable_for(meta, reason.to_owned(), true, false, None, None, None)
    }

    fn restart_limit_error(&self, imports: &[String], limit: RestartLimitExceeded) -> ServerError {
        let snapshot = self.runtime.lock().facts();
        ServerError::worker_unavailable(WorkerUnavailable {
            retryable: false,
            worker_restarted: false,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            project_hash: self.config.manifest_hash.clone(),
            imports: imports.to_vec(),
            session_id: self.config.session_id.clone(),
            lean_toolchain: self.config.toolchain_label.clone(),
            worker_generation: self.observed_generation(),
            reason: limit.message,
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

struct RestartLimitExceeded {
    message: String,
    cause: RestartCause,
    restarts_in_window: u64,
    window_millis: u64,
}

fn actor_main(
    config: ActorConfig,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<ProjectMessage>), ServerError>>,
) {
    let (handle, runtime_toolchain) = match open_worker(&config, true) {
        Ok(value) => value,
        Err(err) => {
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
        profile_switch_count: 0,
        last_rss_kib: None,
        runtime: Arc::clone(&config.runtime),
        abnormal_restart_times: VecDeque::new(),
        restart_stats: RestartStats::default(),
    };

    let (tx, mut rx) = mpsc::channel::<ProjectMessage>(config.mailbox_capacity);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    while let Some(message) = rx.blocking_recv() {
        if !config.healthy.load(Ordering::Acquire) {
            message.reject(&state, "project_shutting_down");
            continue;
        }
        state.handle_message(message);
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
            tracing::warn!(error = %err, "project worker shutdown failed");
        }
    }
}

fn open_worker(config: &ActorConfig, preflight: bool) -> Result<(LeanWorkerHostHandle, String)> {
    let builder = worker_builder(config);
    if preflight {
        let report = builder.check();
        if let Some(first) = report.first_error() {
            return Err(ServerError::BadProject(format!(
                "{}: {}",
                first.code(),
                first.message()
            )));
        }
    }
    // No `RssHardLimitExceeded` arm: this server no longer sets
    // `rss_hard_limit`, so the supervisor cannot produce one. See
    // `LEAN_MAX_MEMORY_KIB`.
    let handle = builder.open().map_err(map_worker_err)?;
    let runtime_toolchain = handle
        .runtime_metadata()
        .lean_version
        .unwrap_or_else(|| config.toolchain_label.clone());
    Ok((handle, runtime_toolchain))
}

fn worker_builder(config: &ActorConfig) -> LeanWorkerHostHandleBuilder {
    // One policy restart, on the one quantity that accumulates without bound:
    // imports. `disabled()`'s own doc warns that a long-running host wants
    // `memory_bounded`, "because fresh imports retain Lean process-global state
    // until the child exits" — session reuse made that true per *import* rather
    // than per *call*, which is what [`WORKER_MAX_IMPORTS`] bounds. No
    // `max_rss_kib`, no `max_requests`, no `rss_hard_limit`. The Lean heap is
    // bounded separately by [`LEAN_MAX_MEMORY_KIB`]; the crash-loop breaker
    // (`MAX_RESTARTS_PER_WINDOW`) and the request deadline are this actor's own.
    let restart_policy = LeanWorkerRestartPolicy::default()
        .max_imports(WORKER_MAX_IMPORTS)
        .max_restarts_per_window(SUPERVISOR_RESTART_INTENSITY, RESTART_WINDOW);
    let module_cache_limits = module_cache_limits();
    LeanWorkerHostHandleBuilder::shims_only(&config.lake_root, std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_millis(config.request_timeout_millis))
        .restart_policy(restart_policy)
        .lean_max_memory_kib(config.lean_max_memory_kib)
        .module_cache_limits(module_cache_limits)
}

fn semantic_capability_builder(
    config: &ActorConfig,
    built: &lean_toolchain::LeanBuiltCapability,
) -> Result<LeanWorkerCapabilityBuilder> {
    // See `worker_builder`: bounded by import count, one Lean heap ceiling.
    let restart_policy = LeanWorkerRestartPolicy::default()
        .max_imports(WORKER_MAX_IMPORTS)
        .max_restarts_per_window(SUPERVISOR_RESTART_INTENSITY, RESTART_WINDOW);
    let builder = LeanWorkerCapabilityBuilder::from_built_capability(built, std::iter::empty::<String>())
        .map_err(map_worker_err)?
        .import_workspace_root(config.lake_root.clone())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_millis(config.request_timeout_millis))
        .restart_policy(restart_policy)
        .lean_max_memory_kib(config.lean_max_memory_kib)
        .module_cache_limits(module_cache_limits())
        .json_command_export(lean_semantic_search_capability::DECLARATION_FEATURES_EXPORT)
        .json_command_export(lean_semantic_search_capability::PROOF_GOAL_FEATURES_EXPORT);
    Ok(builder)
}

/// Built semantic-search runtimes, one per `(toolchain label, Lean sysroot)`.
///
/// `lean_semantic_search_runtime::build_cached` is cached *on disk*, not in
/// process: every call re-materializes the source package, takes a filesystem
/// build lock, and runs a Lake build to conclude there is nothing to do.
/// Measured warm on this machine at **5.5 ms**, paid on the project's actor
/// thread — where it blocks the worker — on every semantic call. The result is
/// a pure function of the key, so it is computed once per process instead.
///
/// Process-wide rather than per project because that is the true scope: two
/// projects pinned to the same toolchain build the identical runtime into the
/// identical cache directory, and today they each pay for it.
///
/// The build itself runs *outside* the lock. Holding it would serialize every
/// project behind one Lake invocation; upstream's own filesystem lock already
/// makes a concurrent duplicate build safe, and the loser simply overwrites an
/// equal value.
static SEMANTIC_RUNTIMES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<(String, PathBuf), lean_semantic_search_runtime::SemanticSearchRuntime>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Memoized [`lean_semantic_search_runtime::build_cached`]; see
/// [`SEMANTIC_RUNTIMES`]. Failures are never cached — a build that failed
/// because a toolchain was mid-install must be retried, not remembered.
///
/// The error is a rendered message rather than a [`ServerError`] so the caller
/// keeps deciding what a failure means; it wraps this in the same structured
/// `worker_unavailable` it always did.
fn semantic_runtime(
    toolchain_label: &str,
    lean_sysroot: &Path,
) -> std::result::Result<lean_semantic_search_runtime::SemanticSearchRuntime, String> {
    let key = (toolchain_label.to_owned(), lean_sysroot.to_path_buf());
    // Bound the guard to a statement so the lock is released before the build
    // below, rather than living to the end of an `if let`.
    let cached = SEMANTIC_RUNTIMES.lock().get(&key).cloned();
    if let Some(runtime) = cached {
        return Ok(runtime);
    }
    let cache_root = semantic_runtime_cache_root().map_err(|err| err.to_string())?;
    let runtime = lean_semantic_search_runtime::build_cached(SemanticSearchRuntimeBuild {
        cache_root,
        toolchain_label: toolchain_label.to_owned(),
        lean_sysroot: lean_sysroot.to_path_buf(),
    })
    .map_err(|err| err.to_string())?;
    SEMANTIC_RUNTIMES.lock().insert(key, runtime.clone());
    Ok(runtime)
}

fn semantic_runtime_cache_root() -> Result<PathBuf> {
    let cache_dir =
        dirs::cache_dir().ok_or_else(|| ServerError::Internal("could not resolve user cache directory".to_owned()))?;
    Ok(cache_dir.join("lean-host-mcp").join("semantic-runtimes"))
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

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "only worker process death variants are restart causes; all other errors are classified elsewhere"
)]
fn worker_death_cause(err: &LeanWorkerError) -> RestartCause {
    match err {
        LeanWorkerError::ChildPanicOrAbort { .. } => RestartCause::ChildAbort,
        LeanWorkerError::ChildExited { .. } => RestartCause::ChildExit,
        _ => RestartCause::WorkerInternal,
    }
}

fn restart_reason_text(reason: &LeanWorkerRestartReason) -> String {
    match reason {
        LeanWorkerRestartReason::Explicit => RestartCause::Explicit.as_str().to_owned(),
        LeanWorkerRestartReason::MaxRequests { limit } => format!("max_requests limit={limit}"),
        LeanWorkerRestartReason::MaxImports { limit } => format!("max_imports limit={limit}"),
        LeanWorkerRestartReason::RssCeiling {
            current_kib, limit_kib, ..
        } => {
            format!("rss_ceiling current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::RssHardLimit {
            operation,
            current_kib,
            limit_kib,
            ..
        } => {
            format!("rss_hard_limit operation={operation} current_kib={current_kib} limit_kib={limit_kib}")
        }
        LeanWorkerRestartReason::Idle { idle_for, limit } => {
            format!(
                "idle idle_for_millis={} limit_millis={}",
                millis_u64(*idle_for),
                millis_u64(*limit)
            )
        }
        LeanWorkerRestartReason::Cancelled { operation } => format!("cancelled operation={operation}"),
        LeanWorkerRestartReason::ChildAbort { operation } => format!("child_abort operation={operation}"),
        LeanWorkerRestartReason::RequestTimeout { operation, duration } => {
            format!(
                "timeout operation={operation} duration_millis={}",
                millis_u64(*duration)
            )
        }
    }
}

fn import_fingerprint(imports: &[String]) -> String {
    imports.join("\n")
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Resolve a knob through `env > file > default` and reject a zero result
/// whatever its source. `env` is the raw env-var string (parsed here); `file`
/// is the already-typed config-file value; `default` is the built-in constant.
fn parse_nonzero_u64(name: &str, env: Option<&str>, file: Option<u64>, default: u64) -> Result<u64> {
    let value = match env {
        Some(raw) => raw
            .parse::<u64>()
            .map_err(|e| ServerError::Internal(format!("{name}={raw:?} not a u64: {e}")))?,
        None => file.unwrap_or(default),
    };
    if value == 0 {
        return Err(ServerError::Internal(format!(
            "{name} resolved to 0, which is not allowed"
        )));
    }
    Ok(value)
}

fn parse_nonzero_usize(name: &str, env: Option<&str>, file: Option<usize>, default: usize) -> Result<usize> {
    let parsed = parse_nonzero_u64(name, env, file.map(|v| v as u64), default as u64)?;
    usize::try_from(parsed).map_err(|_| ServerError::Internal(format!("{name}={parsed} does not fit in usize")))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "unit tests use expect/unwrap_err to state the branch under test directly"
)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_parses_runtime_policy_without_env_reads() {
        // Distinct values so a field routed to the wrong knob is visible.
        let config = parse_runtime_config(
            RuntimeEnv {
                lean_max_memory_kib: Some("5".to_owned()),
                request_timeout_millis: Some("37".to_owned()),
                project_mailbox_capacity: Some("23".to_owned()),
                worker_restart_limit: Some("29".to_owned()),
                worker_restart_window_secs: Some("31".to_owned()),
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap();

        assert_eq!(config.lean_max_memory_kib(), 5);
        assert_eq!(config.request_timeout_millis(), 37);
        assert_eq!(config.mailbox_capacity(), 23);
        assert_eq!(config.max_restarts_per_window(), 29);
        assert_eq!(config.restart_window(), Duration::from_secs(31));
    }

    #[test]
    fn request_timeout_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            request_timeout_millis: Some(45_000),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.request_timeout_millis(), 45_000);
        // Env set -> env wins over the file.
        let env = RuntimeEnv {
            request_timeout_millis: Some("90000".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.request_timeout_millis(), 90_000);
        // Neither -> built-in default (120 s).
        let config = parse_runtime_config(RuntimeEnv::default(), &RuntimeFileConfig::default()).unwrap();
        assert_eq!(config.request_timeout_millis(), REQUEST_TIMEOUT_MILLIS);
    }

    #[test]
    fn request_timeout_zero_is_rejected() {
        // A zero deadline would time every call out instantly; parse_nonzero_u64
        // must reject it.
        let err = parse_runtime_config(
            RuntimeEnv {
                request_timeout_millis: Some("0".to_owned()),
                ..RuntimeEnv::default()
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(
            message.contains("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS"),
            "message: {message}"
        );
    }

    #[test]
    fn runtime_config_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            lean_max_memory_kib: Some(8_388_608),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 8_388_608);
        // Env set -> env wins over the file.
        let env = RuntimeEnv {
            lean_max_memory_kib: Some("6291456".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.lean_max_memory_kib(), 6_291_456);
        // Neither -> built-in default.
        let config = parse_runtime_config(RuntimeEnv::default(), &RuntimeFileConfig::default()).unwrap();
        assert_eq!(config.lean_max_memory_kib(), LEAN_MAX_MEMORY_KIB);
    }

    #[test]
    fn runtime_config_rejects_zero_from_file() {
        // Zero is rejected wherever it comes from: an unbounded Lean heap is a
        // different posture from a large one, and reaching it by typing `0`
        // into a config file is never deliberate.
        let zero = RuntimeFileConfig {
            lean_max_memory_kib: Some(0),
            ..RuntimeFileConfig::default()
        };
        let err = parse_runtime_config(RuntimeEnv::default(), &zero).unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(
            message.contains("LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB"),
            "message: {message}"
        );
    }

    #[test]
    fn restart_stats_tally_total_and_per_cause() {
        let mut stats = RestartStats::default();
        stats.observe("rss_post_job");
        stats.observe("rss_post_job");
        stats.observe("child_abort");

        assert_eq!(stats.total, 3);
        assert_eq!(stats.by_cause.get("rss_post_job"), Some(&2));
        assert_eq!(stats.by_cause.get("child_abort"), Some(&1));
        assert_eq!(stats.by_cause.get("idle"), None);
    }

    #[test]
    fn semantic_capability_builder_omits_capability_import_module() {
        let tmp = tempfile::tempdir().unwrap();
        let capability_root = tmp.path().join("capability");
        let lib_dir = capability_root.join(".lake").join("build").join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let dylib_name = if cfg!(target_os = "macos") {
            "libLeanSemanticSearch.dylib"
        } else {
            "libLeanSemanticSearch.so"
        };
        let dylib = lib_dir.join(dylib_name);
        std::fs::write(&dylib, "").unwrap();
        let built = lean_toolchain::LeanBuiltCapability::path(&dylib)
            .package("lean_semantic_search")
            .module("LeanSemanticSearch");
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
        }));
        let config = ActorConfig {
            lake_root: tmp.path().join("consumer"),
            manifest_hash: "sha256-test".to_owned(),
            toolchain_label: "leanprover/lean4:test".to_owned(),
            worker_path: tmp.path().join("worker"),
            lean_sysroot: tmp.path().join("lean"),
            session_id: "session-test".to_owned(),
            runtime,
            healthy: Arc::new(AtomicBool::new(true)),
            artifact_roots: Vec::new(),
            lean_max_memory_kib: LEAN_MAX_MEMORY_KIB,
            request_timeout_millis: REQUEST_TIMEOUT_MILLIS,
            mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
            max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
            restart_window: RESTART_WINDOW,
            toolchain_advisories: Vec::new(),
        };

        let builder = semantic_capability_builder(&config, &built).unwrap();
        let debug = format!("{builder:?}");

        assert!(debug.contains("imports: []"), "builder debug: {debug}");
        assert!(
            !debug.contains("LeanSemanticSearch.Capability"),
            "builder must not import the capability module: {debug}"
        );
        assert!(
            debug.contains("import_workspace_root: Some"),
            "builder should import sessions against the consumer workspace: {debug}"
        );
    }

    #[test]
    fn planned_restart_causes_do_not_consume_abnormal_restart_budget() {
        assert!(!RestartCause::RssPostJob.counts_toward_restart_limit());
        assert!(!RestartCause::MaxRequests.counts_toward_restart_limit());
        assert!(!RestartCause::MaxImports.counts_toward_restart_limit());
        assert!(!RestartCause::Idle.counts_toward_restart_limit());

        assert!(RestartCause::ChildExit.counts_toward_restart_limit());
        assert!(RestartCause::ChildAbort.counts_toward_restart_limit());
        assert!(RestartCause::Timeout.counts_toward_restart_limit());
        assert!(RestartCause::Cancelled.counts_toward_restart_limit());
        assert!(RestartCause::SessionMissing.counts_toward_restart_limit());
        assert!(RestartCause::RssHardLimit.counts_toward_restart_limit());
    }

    #[test]
    fn worker_restart_reason_maps_to_stable_cause() {
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::MaxRequests { limit: 1 }).as_str(),
            "max_requests"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssCeiling {
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_post_job"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RssHardLimit {
                operation: "test",
                current_kib: 2,
                limit_kib: 1,
                last_import_stats: None,
            })
            .as_str(),
            "rss_hard_limit_exceeded"
        );
        assert_eq!(
            restart_cause_from_worker(&LeanWorkerRestartReason::RequestTimeout {
                operation: "test",
                duration: Duration::from_millis(1),
            })
            .as_str(),
            "timeout"
        );
    }
}
