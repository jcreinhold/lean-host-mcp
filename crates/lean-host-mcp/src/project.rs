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

use crate::admission::SemanticPermit;
use crate::cache::ModuleQueryCache;
use crate::config_file::RuntimeFileConfig;
use crate::envelope::{Freshness, RuntimeFacts, RuntimeRestartEvent};
use crate::error::{Result, ServerError, WorkerUnavailable, map_worker_err};
use crate::lake_meta::LakeProjectMeta;
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};
use crate::toolchain::{Readiness, ToolchainId, WorkerBinary};

/// LRU capacity for exact bounded module query results.
const MODULE_QUERY_CACHE_CAPACITY: usize = 256;
const WORKER_REQUEST_RESTARTS: u64 = 64;
const PROJECT_MAILBOX_CAPACITY: usize = 8;
const WORKER_RSS_POST_JOB_RESTART_KIB: u64 = 5 * 1024 * 1024;
const WORKER_RSS_HARD_KILL_KIB: u64 = 16 * 1024 * 1024;
const WORKER_RSS_SAMPLE_MILLIS: u64 = 250;
const IMPORT_SWITCH_RSS_SOFT_KIB: u64 = 2 * 1024 * 1024;
const MODULE_CACHE_RSS_GUARD_KIB: u64 = 2 * 1024 * 1024;
const MODULE_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Per-request worker deadline. Covers one tool call end to end (live rows,
/// diagnostics, terminal response); on expiry the worker is recycled and the
/// call returns a retryable runtime error. Replaces the worker-parent's 10-min
/// `long_running_requests` profile, which let whole-project scans (e.g.
/// `find_references` at project scope) appear to hang. Raise it for unusually
/// heavy modules whose `verify`/`proof_state` legitimately runs longer.
const REQUEST_TIMEOUT_MILLIS: u64 = 120 * 1000;
const MAX_JOB_RETRIES: u32 = 1;
const MAX_RESTARTS_PER_WINDOW: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_mins(1);

/// Runtime policy for one private project actor.
///
/// The binary parses this once at server startup and passes it into the
/// broker. Tests and embedders can construct the default directly without
/// rereading process environment during project open.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectRuntimeConfig {
    worker_rss_post_job_restart_kib: u64,
    worker_rss_hard_kill_kib: u64,
    worker_rss_sample_millis: u64,
    import_switch_rss_soft_kib: u64,
    module_cache_rss_guard_kib: u64,
    module_cache_max_bytes: u64,
    request_timeout_millis: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
}

impl Default for ProjectRuntimeConfig {
    fn default() -> Self {
        Self {
            worker_rss_post_job_restart_kib: WORKER_RSS_POST_JOB_RESTART_KIB,
            worker_rss_hard_kill_kib: WORKER_RSS_HARD_KILL_KIB,
            worker_rss_sample_millis: WORKER_RSS_SAMPLE_MILLIS,
            import_switch_rss_soft_kib: IMPORT_SWITCH_RSS_SOFT_KIB,
            module_cache_rss_guard_kib: MODULE_CACHE_RSS_GUARD_KIB,
            module_cache_max_bytes: MODULE_CACHE_MAX_BYTES,
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
    /// value (from env or file) is zero where zero is unsafe, or the RSS
    /// ceilings violate `import_switch <= post_job <= hard_kill`.
    pub fn from_env_with_file(file: &RuntimeFileConfig) -> Result<Self> {
        parse_runtime_config(
            RuntimeEnv {
                worker_rss_post_job_restart_kib: runtime_env_var("LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB")?,
                worker_rss_hard_kill_kib: runtime_env_var("LEAN_HOST_MCP_WORKER_RSS_HARD_KILL_KIB")?,
                worker_rss_sample_millis: runtime_env_var("LEAN_HOST_MCP_WORKER_RSS_SAMPLE_MILLIS")?,
                import_switch_rss_soft_kib: runtime_env_var("LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB")?,
                module_cache_rss_guard_kib: runtime_env_var("LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB")?,
                module_cache_max_bytes: runtime_env_var("LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES")?,
                request_timeout_millis: runtime_env_var("LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS")?,
                project_mailbox_capacity: runtime_env_var("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY")?,
                worker_restart_limit: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_LIMIT")?,
                worker_restart_window_secs: runtime_env_var("LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS")?,
            },
            file,
        )
    }

    #[must_use]
    pub const fn worker_rss_post_job_restart_kib(&self) -> u64 {
        self.worker_rss_post_job_restart_kib
    }

    #[must_use]
    pub const fn worker_rss_hard_kill_kib(&self) -> u64 {
        self.worker_rss_hard_kill_kib
    }

    #[must_use]
    pub const fn worker_rss_sample_millis(&self) -> u64 {
        self.worker_rss_sample_millis
    }

    #[must_use]
    pub const fn import_switch_rss_soft_kib(&self) -> u64 {
        self.import_switch_rss_soft_kib
    }

    #[must_use]
    pub const fn module_cache_rss_guard_kib(&self) -> u64 {
        self.module_cache_rss_guard_kib
    }

    #[must_use]
    pub const fn module_cache_max_bytes(&self) -> u64 {
        self.module_cache_max_bytes
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
    worker_rss_post_job_restart_kib: Option<String>,
    worker_rss_hard_kill_kib: Option<String>,
    worker_rss_sample_millis: Option<String>,
    import_switch_rss_soft_kib: Option<String>,
    module_cache_rss_guard_kib: Option<String>,
    module_cache_max_bytes: Option<String>,
    request_timeout_millis: Option<String>,
    project_mailbox_capacity: Option<String>,
    worker_restart_limit: Option<String>,
    worker_restart_window_secs: Option<String>,
}

fn parse_runtime_config(env: RuntimeEnv, file: &RuntimeFileConfig) -> Result<ProjectRuntimeConfig> {
    let defaults = ProjectRuntimeConfig::default();
    let config = ProjectRuntimeConfig {
        worker_rss_post_job_restart_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB",
            env.worker_rss_post_job_restart_kib.as_deref(),
            file.worker_rss_post_job_restart_kib,
            defaults.worker_rss_post_job_restart_kib,
        )?,
        worker_rss_hard_kill_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_WORKER_RSS_HARD_KILL_KIB",
            env.worker_rss_hard_kill_kib.as_deref(),
            file.worker_rss_hard_kill_kib,
            defaults.worker_rss_hard_kill_kib,
        )?,
        worker_rss_sample_millis: parse_nonzero_u64(
            "LEAN_HOST_MCP_WORKER_RSS_SAMPLE_MILLIS",
            env.worker_rss_sample_millis.as_deref(),
            file.worker_rss_sample_millis,
            defaults.worker_rss_sample_millis,
        )?,
        import_switch_rss_soft_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB",
            env.import_switch_rss_soft_kib.as_deref(),
            file.import_switch_rss_soft_kib,
            defaults.import_switch_rss_soft_kib,
        )?,
        module_cache_rss_guard_kib: parse_nonzero_u64(
            "LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB",
            env.module_cache_rss_guard_kib.as_deref(),
            file.module_cache_rss_guard_kib,
            defaults.module_cache_rss_guard_kib,
        )?,
        module_cache_max_bytes: parse_nonzero_u64(
            "LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES",
            env.module_cache_max_bytes.as_deref(),
            file.module_cache_max_bytes,
            defaults.module_cache_max_bytes,
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
    validate_rss_ordering(&config)?;
    Ok(config)
}

/// The three RSS ceilings escalate: a worker cycles cleanly before an
/// import-profile switch (`import_switch`), again after a job that grew past the
/// post-job budget (`post_job`), and is killed in-flight only at the hard limit
/// (`hard_kill`). If a tuned value inverts that order the cheaper cycle can
/// never fire — e.g. `post_job > hard_kill` means the planned post-job recycle
/// is unreachable and every overrun escalates straight to a hard kill. Reject it
/// at startup with the offending values, rather than degrade silently.
fn validate_rss_ordering(config: &ProjectRuntimeConfig) -> Result<()> {
    if config.import_switch_rss_soft_kib > config.worker_rss_post_job_restart_kib {
        return Err(ServerError::Internal(format!(
            "invalid RSS config: import_switch={} KiB exceeds post_job={} KiB \
             (need import_switch <= post_job <= hard_kill)",
            config.import_switch_rss_soft_kib, config.worker_rss_post_job_restart_kib,
        )));
    }
    if config.worker_rss_post_job_restart_kib > config.worker_rss_hard_kill_kib {
        return Err(ServerError::Internal(format!(
            "invalid RSS config: post_job={} KiB exceeds hard_kill={} KiB \
             (need import_switch <= post_job <= hard_kill)",
            config.worker_rss_post_job_restart_kib, config.worker_rss_hard_kill_kib,
        )));
    }
    Ok(())
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
    admission_wait_millis: u64,
    _correlation_id: uuid::Uuid,
    retry_policy: RetryPolicy,
    _active_job: ActiveJobGuard,
    _semantic_permit: SemanticPermit,
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
    DeclarationSearch {
        meta: JobMeta,
        request: LeanWorkerDeclarationSearch,
        reply: oneshot::Sender<Result<ProjectCall<LeanWorkerDeclarationSearchResult>>>,
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
    #[expect(dead_code, reason = "stable wire cause reserved for future non-RSS profile cycling")]
    ImportProfileSwitch,
    RssImportSwitch,
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
            Self::ImportProfileSwitch => "import_profile_switch",
            Self::RssImportSwitch => "rss_import_switch",
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
        "rss_post_job" | "rss_import_switch" => emit!(info, "worker recycled (memory pressure)"),
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
            admission_wait_millis: 0,
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
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQuery {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
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
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn process_module_query_batch(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQueryBatch {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
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
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn inspect_declaration(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: LeanWorkerDeclarationInspectionRequest,
    ) -> Result<ProjectCall<LeanWorkerDeclarationInspectionResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationInspection {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
            request,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Run bounded declaration search through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn search_declarations(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: LeanWorkerDeclarationSearch,
    ) -> Result<ProjectCall<LeanWorkerDeclarationSearchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationSearch {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
            request,
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
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: SemanticProofSearchRequest,
    ) -> Result<ProjectCall<SemanticProofSearchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::SemanticProofSearch {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
            request,
            reply,
        };
        self.enqueue(message, rx).await
    }

    /// Try proof fragments in-memory through this project's serialized worker actor.
    ///
    /// # Errors
    ///
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn attempt_proof(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerProofAttemptResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ProofAttempt {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
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
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerification {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
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
    /// Returns `ServerError` when admission, mailbox enqueue, actor reply, or
    /// worker execution fails.
    pub(crate) async fn verify_declaration_batch(
        &self,
        imports: Vec<String>,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationBatchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerificationBatch {
            meta: self.job_meta(
                imports,
                RetryPolicy::RetryOnceReadOnly,
                semantic_permit,
                admission_wait_millis,
            ),
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    fn job_meta(
        &self,
        imports: Vec<String>,
        retry_policy: RetryPolicy,
        semantic_permit: SemanticPermit,
        admission_wait_millis: u64,
    ) -> JobMeta {
        let created_at = Instant::now();
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        JobMeta {
            import_fingerprint: import_fingerprint(&imports),
            imports,
            _created_at: created_at,
            queued_at: Instant::now(),
            admission_wait_millis,
            _correlation_id: uuid::Uuid::new_v4(),
            retry_policy,
            _active_job: ActiveJobGuard {
                active_jobs: Arc::clone(&self.active_jobs),
            },
            _semantic_permit: semantic_permit,
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
    worker_rss_post_job_restart_kib: u64,
    worker_rss_hard_kill_kib: u64,
    worker_rss_sample_millis: u64,
    import_switch_rss_soft_kib: u64,
    module_cache_rss_guard_kib: u64,
    module_cache_max_bytes: u64,
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
            worker_rss_post_job_restart_kib: runtime_config.worker_rss_post_job_restart_kib(),
            worker_rss_hard_kill_kib: runtime_config.worker_rss_hard_kill_kib(),
            worker_rss_sample_millis: runtime_config.worker_rss_sample_millis(),
            import_switch_rss_soft_kib: runtime_config.import_switch_rss_soft_kib(),
            module_cache_rss_guard_kib: runtime_config.module_cache_rss_guard_kib(),
            module_cache_max_bytes: runtime_config.module_cache_max_bytes(),
            request_timeout_millis: runtime_config.request_timeout_millis(),
            mailbox_capacity: runtime_config.mailbox_capacity(),
            max_restarts_per_window: runtime_config.max_restarts_per_window(),
            restart_window: runtime_config.restart_window(),
            toolchain_advisories: open_warnings.clone(),
        };
        Ok((config, open_warnings))
    }
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    worker_generation_base: u64,
    last_restart: Option<RuntimeRestartEvent>,
    last_import_fingerprint: Option<String>,
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
            ProjectMessage::DeclarationSearch { meta, request, reply } => {
                let result = self.run_job(meta, |handle, imports| {
                    handle.search_declarations_with_imports(imports, &request, None, None)
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
        let mut call_restart = self.cycle_before_import_switch_if_needed(&meta)?;
        let mut lifecycle_baseline = self.handle.lifecycle_snapshot();

        let max_retries = meta.retry_policy.retries();
        let mut retry_count = 0_u32;
        loop {
            match job(&mut self.handle, meta.imports.clone()) {
                Ok(value) => {
                    if let Some(event) = self.account_lifecycle_restarts_since(&lifecycle_baseline, &meta)? {
                        call_restart = Some(event);
                    }
                    self.last_import_fingerprint = Some(meta.import_fingerprint.clone());
                    self.last_rss_kib = self.handle.rss_kib().or(self.last_rss_kib);
                    if let Some(event) = self.cycle_after_post_job_rss_if_needed(&meta)? {
                        call_restart = Some(event);
                    }
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
                    self.last_import_fingerprint = Some(meta.import_fingerprint.clone());
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

    fn open_semantic_capability(&self, meta: &JobMeta) -> Result<lean_rs_worker_parent::LeanWorkerCapability> {
        let runtime = lean_semantic_search_runtime::build_cached(SemanticSearchRuntimeBuild {
            cache_root: semantic_runtime_cache_root()?,
            toolchain_label: self.config.toolchain_label.clone(),
            lean_sysroot: self.config.lean_sysroot.clone(),
        })
        .map_err(|err| {
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

    fn cycle_before_import_switch_if_needed(&mut self, meta: &JobMeta) -> Result<Option<RuntimeRestartEvent>> {
        let Some(previous_import_fingerprint) = self.last_import_fingerprint.as_deref() else {
            return Ok(None);
        };
        if previous_import_fingerprint == meta.import_fingerprint {
            return Ok(None);
        }
        self.profile_switch_count = self.profile_switch_count.saturating_add(1);
        let Some(current_kib) = self.handle.rss_kib() else {
            return Ok(None);
        };
        self.last_rss_kib = Some(current_kib);
        if current_kib < self.config.import_switch_rss_soft_kib {
            return Ok(None);
        }
        let limit_kib = self.config.import_switch_rss_soft_kib;
        let reason = format!("rss_import_switch current_kib={current_kib} limit_kib={limit_kib}");
        self.record_restart_or_stop(RestartCause::RssImportSwitch, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        self.handle.cycle().map_err(map_worker_err)?;
        self.last_rss_kib = self.handle.rss_kib().or(Some(current_kib));
        let event = restart_event(
            RestartCause::RssImportSwitch,
            reason,
            self.observed_generation(),
            Some(current_kib),
            Some(limit_kib),
        );
        self.record_restart(event.clone());
        Ok(Some(event))
    }

    fn cycle_after_post_job_rss_if_needed(&mut self, meta: &JobMeta) -> Result<Option<RuntimeRestartEvent>> {
        let Some(current_kib) = self.handle.rss_kib() else {
            return Ok(None);
        };
        self.last_rss_kib = Some(current_kib);
        let limit_kib = self.config.worker_rss_post_job_restart_kib;
        tracing::debug!(rss_kib = current_kib, limit_kib, "post-job rss check");
        if current_kib < limit_kib {
            return Ok(None);
        }
        let reason = format!("rss_post_job current_kib={current_kib} limit_kib={limit_kib}");
        self.record_restart_or_stop(RestartCause::RssPostJob, &reason)
            .map_err(|limit| self.restart_limit_error(&meta.imports, limit))?;
        self.handle.cycle().map_err(map_worker_err)?;
        self.last_rss_kib = self.handle.rss_kib().or(Some(current_kib));
        let event = restart_event(
            RestartCause::RssPostJob,
            reason,
            self.observed_generation(),
            Some(current_kib),
            Some(limit_kib),
        );
        self.record_restart(event.clone());
        Ok(Some(event))
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
                admission_wait_millis: 0,
                queue_wait_millis: 0,
                call_restart: None,
                last_restart: Some(event),
                rss_kib: self.last_rss_kib,
                worker_lanes: 1,
                import_profile: self.last_import_fingerprint.clone(),
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
            admission_wait_millis: meta.admission_wait_millis,
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
        last_import_fingerprint: None,
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
            if bootstrap_failure_is_hard_rss(&first.code().to_string(), first.message()) {
                return Err(bootstrap_hard_rss_unavailable(
                    config,
                    first.message().to_owned(),
                    parse_keyed_u64(first.message(), "current_kib"),
                    parse_keyed_u64(first.message(), "limit_kib").or(Some(config.worker_rss_hard_kill_kib)),
                ));
            }
            return Err(ServerError::BadProject(format!(
                "{}: {}",
                first.code(),
                first.message()
            )));
        }
    }
    let handle = match builder.open() {
        Ok(handle) => handle,
        Err(LeanWorkerError::RssHardLimitExceeded {
            operation,
            current_kib,
            limit_kib,
            ..
        }) => {
            return Err(bootstrap_hard_rss_unavailable(
                config,
                format!(
                    "rss_hard_limit_exceeded operation={operation} current_kib={current_kib} limit_kib={limit_kib}"
                ),
                Some(current_kib),
                Some(limit_kib),
            ));
        }
        Err(err) => return Err(map_worker_err(err)),
    };
    let runtime_toolchain = handle
        .runtime_metadata()
        .lean_version
        .unwrap_or_else(|| config.toolchain_label.clone());
    Ok((handle, runtime_toolchain))
}

fn bootstrap_failure_is_hard_rss(code: &str, message: &str) -> bool {
    let lower = message.to_lowercase();
    code.contains("rss")
        || lower.contains("hard rss limit")
        || lower.contains("rss_hard_limit")
        || lower.contains("rss_hard_limit_exceeded")
        || lower.contains("rss hard limit")
}

fn bootstrap_hard_rss_unavailable(
    config: &ActorConfig,
    reason: String,
    current_kib: Option<u64>,
    limit_kib: Option<u64>,
) -> ServerError {
    config.healthy.store(false, Ordering::Release);
    let generation = config.runtime.lock().worker_generation;
    let event = restart_event(
        RestartCause::RssHardLimit,
        reason.clone(),
        generation,
        current_kib,
        limit_kib,
    );
    // First-spawn worker tripped the hard RSS limit before serving a call: a
    // single terminal recycle. Emit the same log line a live recycle would, so
    // the cause is visible on stderr even when the project never opens.
    let restarts_total = 1;
    log_restart(&event, restarts_total);
    let by_cause = BTreeMap::from([(RestartCause::RssHardLimit.as_str().to_owned(), restarts_total)]);
    let runtime = RuntimeFacts {
        worker_generation: generation,
        worker_restarted: true,
        retry_count: 0,
        admission_wait_millis: 0,
        queue_wait_millis: 0,
        call_restart: Some(event.clone()),
        last_restart: Some(event.clone()),
        rss_kib: current_kib,
        worker_lanes: 1,
        import_profile: None,
        profile_switch_count: 0,
        restarts_total,
        restarts_by_cause: by_cause.clone(),
    };
    *config.runtime.lock() = RuntimeSnapshot {
        worker_generation: generation,
        last_restart: Some(event),
        rss_kib: current_kib,
        import_profile: None,
        profile_switch_count: 0,
        restarts_total,
        restarts_by_cause: by_cause,
    };
    ServerError::worker_unavailable(WorkerUnavailable {
        retryable: false,
        worker_restarted: true,
        project_root: config.lake_root.to_string_lossy().into_owned(),
        project_hash: config.manifest_hash.clone(),
        imports: Vec::new(),
        session_id: config.session_id.clone(),
        lean_toolchain: config.toolchain_label.clone(),
        worker_generation: generation,
        reason,
        restart_cause: Some(RestartCause::RssHardLimit.as_str().to_owned()),
        rss_kib: current_kib,
        limit_kib,
        retry_after_millis: None,
        restarts_in_window: None,
        window_millis: None,
        runtime,
        toolchain_advisories: config.toolchain_advisories.clone(),
    })
}

fn worker_builder(config: &ActorConfig) -> LeanWorkerHostHandleBuilder {
    let restart_policy = LeanWorkerRestartPolicy::default().max_requests(WORKER_REQUEST_RESTARTS);
    let module_cache_limits = module_cache_limits(config);
    LeanWorkerHostHandleBuilder::shims_only(&config.lake_root, std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .request_timeout(Duration::from_millis(config.request_timeout_millis))
        .restart_policy(restart_policy)
        .rss_hard_limit(
            config.worker_rss_hard_kill_kib,
            Duration::from_millis(config.worker_rss_sample_millis),
        )
        .module_cache_limits(module_cache_limits)
}

fn semantic_capability_builder(
    config: &ActorConfig,
    built: &lean_toolchain::LeanBuiltCapability,
) -> Result<LeanWorkerCapabilityBuilder> {
    let restart_policy = LeanWorkerRestartPolicy::default().max_requests(WORKER_REQUEST_RESTARTS);
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
        .rss_hard_limit(
            config.worker_rss_hard_kill_kib,
            Duration::from_millis(config.worker_rss_sample_millis),
        )
        .module_cache_limits(module_cache_limits(config))
        .json_command_export(lean_semantic_search_capability::DECLARATION_FEATURES_EXPORT)
        .json_command_export(lean_semantic_search_capability::PROOF_GOAL_FEATURES_EXPORT);
    Ok(builder)
}

fn semantic_runtime_cache_root() -> Result<PathBuf> {
    let cache_dir =
        dirs::cache_dir().ok_or_else(|| ServerError::Internal("could not resolve user cache directory".to_owned()))?;
    Ok(cache_dir.join("lean-host-mcp").join("semantic-runtimes"))
}

fn module_cache_limits(config: &ActorConfig) -> LeanWorkerModuleCacheLimits {
    LeanWorkerModuleCacheLimits::default()
        .rss_guard_kib(config.module_cache_rss_guard_kib)
        .max_bytes(config.module_cache_max_bytes)
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

fn parse_keyed_u64(text: &str, key: &str) -> Option<u64> {
    let prefix = format!("{key}=");
    let start = text.find(&prefix)?.checked_add(prefix.len())?;
    let digits = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
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
        // Distinct values that also satisfy the RSS ordering invariant
        // (import_switch <= post_job <= hard_kill); see validate_rss_ordering.
        let config = parse_runtime_config(
            RuntimeEnv {
                worker_rss_post_job_restart_kib: Some("5".to_owned()),
                worker_rss_hard_kill_kib: Some("7".to_owned()),
                worker_rss_sample_millis: Some("11".to_owned()),
                import_switch_rss_soft_kib: Some("3".to_owned()),
                module_cache_rss_guard_kib: Some("17".to_owned()),
                module_cache_max_bytes: Some("19".to_owned()),
                request_timeout_millis: Some("37".to_owned()),
                project_mailbox_capacity: Some("23".to_owned()),
                worker_restart_limit: Some("29".to_owned()),
                worker_restart_window_secs: Some("31".to_owned()),
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap();

        assert_eq!(config.worker_rss_post_job_restart_kib(), 5);
        assert_eq!(config.worker_rss_hard_kill_kib(), 7);
        assert_eq!(config.worker_rss_sample_millis(), 11);
        assert_eq!(config.import_switch_rss_soft_kib(), 3);
        assert_eq!(config.module_cache_rss_guard_kib(), 17);
        assert_eq!(config.module_cache_max_bytes(), 19);
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
    fn rss_config_rejects_post_job_above_hard_kill() {
        let err = parse_runtime_config(
            RuntimeEnv {
                // 20 GiB post-job ceiling above the 16 GiB default hard kill: the
                // planned post-job cycle could never fire before the hard kill.
                worker_rss_post_job_restart_kib: Some("20971520".to_owned()),
                ..RuntimeEnv::default()
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(message.contains("invalid RSS config"), "message: {message}");
        assert!(message.contains("hard_kill"), "message: {message}");
    }

    #[test]
    fn rss_config_rejects_import_switch_above_post_job() {
        let err = parse_runtime_config(
            RuntimeEnv {
                // Import-switch soft limit above the 5 GiB default post-job ceiling.
                import_switch_rss_soft_kib: Some("6291456".to_owned()),
                ..RuntimeEnv::default()
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(message.contains("invalid RSS config"), "message: {message}");
        assert!(message.contains("import_switch"), "message: {message}");
    }

    #[test]
    fn rss_config_accepts_raising_post_job_to_8gib() {
        // The motivating case: 8 GiB post-job ceiling, below the 16 GiB hard
        // kill and above the 2 GiB import-switch soft limit.
        let config = parse_runtime_config(
            RuntimeEnv {
                worker_rss_post_job_restart_kib: Some("8388608".to_owned()),
                ..RuntimeEnv::default()
            },
            &RuntimeFileConfig::default(),
        )
        .unwrap();
        assert_eq!(config.worker_rss_post_job_restart_kib(), 8_388_608);
    }

    #[test]
    fn runtime_config_precedence_env_over_file_over_default() {
        let file = RuntimeFileConfig {
            worker_rss_post_job_restart_kib: Some(8_388_608),
            ..RuntimeFileConfig::default()
        };
        // Env unset -> file value is used.
        let config = parse_runtime_config(RuntimeEnv::default(), &file).unwrap();
        assert_eq!(config.worker_rss_post_job_restart_kib(), 8_388_608);
        // Env set -> env wins over the file (6 GiB, still a valid ordering).
        let env = RuntimeEnv {
            worker_rss_post_job_restart_kib: Some("6291456".to_owned()),
            ..RuntimeEnv::default()
        };
        let config = parse_runtime_config(env, &file).unwrap();
        assert_eq!(config.worker_rss_post_job_restart_kib(), 6_291_456);
        // Neither -> built-in default.
        let config = parse_runtime_config(RuntimeEnv::default(), &RuntimeFileConfig::default()).unwrap();
        assert_eq!(
            config.worker_rss_post_job_restart_kib(),
            WORKER_RSS_POST_JOB_RESTART_KIB
        );
    }

    #[test]
    fn runtime_config_rejects_zero_and_bad_ordering_from_file() {
        let zero = RuntimeFileConfig {
            worker_rss_sample_millis: Some(0),
            ..RuntimeFileConfig::default()
        };
        assert!(parse_runtime_config(RuntimeEnv::default(), &zero).is_err());

        let inverted = RuntimeFileConfig {
            worker_rss_post_job_restart_kib: Some(20_971_520), // above 16 GiB default hard kill
            ..RuntimeFileConfig::default()
        };
        let err = parse_runtime_config(RuntimeEnv::default(), &inverted).unwrap_err();
        let ServerError::Internal(message) = err else {
            panic!("expected Internal config error");
        };
        assert!(message.contains("invalid RSS config"), "message: {message}");
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
            worker_rss_post_job_restart_kib: WORKER_RSS_POST_JOB_RESTART_KIB,
            worker_rss_hard_kill_kib: WORKER_RSS_HARD_KILL_KIB,
            worker_rss_sample_millis: WORKER_RSS_SAMPLE_MILLIS,
            import_switch_rss_soft_kib: IMPORT_SWITCH_RSS_SOFT_KIB,
            module_cache_rss_guard_kib: MODULE_CACHE_RSS_GUARD_KIB,
            module_cache_max_bytes: MODULE_CACHE_MAX_BYTES,
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
    fn bootstrap_hard_rss_failure_is_structured_runtime_unavailable() {
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
            lake_root: PathBuf::from("/tmp/lean-host-mcp-bootstrap-rss-test"),
            manifest_hash: "sha256-test".to_owned(),
            toolchain_label: "leanprover/lean4:test".to_owned(),
            worker_path: PathBuf::from("/tmp/worker"),
            lean_sysroot: PathBuf::from("/tmp/lean"),
            session_id: "session-test".to_owned(),
            runtime: Arc::clone(&runtime),
            healthy: Arc::new(AtomicBool::new(true)),
            worker_rss_post_job_restart_kib: WORKER_RSS_POST_JOB_RESTART_KIB,
            worker_rss_hard_kill_kib: 64,
            worker_rss_sample_millis: WORKER_RSS_SAMPLE_MILLIS,
            import_switch_rss_soft_kib: IMPORT_SWITCH_RSS_SOFT_KIB,
            module_cache_rss_guard_kib: MODULE_CACHE_RSS_GUARD_KIB,
            module_cache_max_bytes: MODULE_CACHE_MAX_BYTES,
            request_timeout_millis: REQUEST_TIMEOUT_MILLIS,
            mailbox_capacity: PROJECT_MAILBOX_CAPACITY,
            max_restarts_per_window: MAX_RESTARTS_PER_WINDOW,
            restart_window: RESTART_WINDOW,
            toolchain_advisories: Vec::new(),
        };

        let err = bootstrap_hard_rss_unavailable(
            &config,
            "rss_hard_limit_exceeded operation=startup current_kib=128 limit_kib=64".to_owned(),
            Some(128),
            Some(64),
        );

        let ServerError::WorkerUnavailable(info) = err else {
            panic!("expected WorkerUnavailable");
        };
        assert!(!info.retryable);
        assert_eq!(info.restart_cause.as_deref(), Some("rss_hard_limit_exceeded"));
        assert_eq!(info.rss_kib, Some(128));
        assert_eq!(info.limit_kib, Some(64));
        assert_eq!(
            info.runtime.last_restart.as_ref().map(|event| event.cause.as_str()),
            Some("rss_hard_limit_exceeded")
        );
    }

    #[test]
    fn planned_restart_causes_do_not_consume_abnormal_restart_budget() {
        assert!(!RestartCause::RssImportSwitch.counts_toward_restart_limit());
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
