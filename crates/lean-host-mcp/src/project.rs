//! `LeanProject`—the unit of Lean semantic execution.
//!
//! One Lake project owns one private actor. The actor serializes all semantic
//! worker calls, owns the child-process supervisor, applies memory/restart
//! policy, and exposes only typed request/reply calls to tool modules. Worker
//! handles, channels, queue internals, and restart mechanics stay below this
//! boundary.

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerDeclarationInspectionRequest, LeanWorkerDeclarationInspectionResult,
    LeanWorkerDeclarationSearch, LeanWorkerDeclarationSearchResult, LeanWorkerDeclarationVerificationRequest,
    LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions, LeanWorkerError, LeanWorkerHostHandle,
    LeanWorkerHostHandleBuilder, LeanWorkerLifecycleSnapshot, LeanWorkerModuleCacheLimits, LeanWorkerModuleQuery,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector,
    LeanWorkerOutputBudgets, LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult, LeanWorkerRestartPolicy,
};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError, WorkerUnavailable, map_worker_err};
use crate::lake_meta::LakeProjectMeta;
use crate::toolchain::{ToolchainId, WorkerBinary};

/// LRU capacity for exact bounded module query results.
const MODULE_QUERY_CACHE_CAPACITY: usize = 256;
const WORKER_REQUEST_RESTARTS: u64 = 64;
const PROJECT_MAILBOX_CAPACITY: usize = 8;
const WORKER_RSS_CEILING_KIB: u64 = 3 * 1024 * 1024;
const MODULE_CACHE_RSS_GUARD_KIB: u64 = 2 * 1024 * 1024;
const MODULE_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_JOB_RETRIES: u32 = 1;
const MAX_RESTARTS_PER_WINDOW: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_mins(1);

/// Result of one project actor call.
#[derive(Debug, Clone)]
pub(crate) struct ProjectCall<T> {
    pub value: T,
    pub runtime: RuntimeFacts,
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
    _semantic_permit: OwnedSemaphorePermit,
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
}

/// Process-wide async admission for heavy Lean semantic work.
#[derive(Debug)]
pub(crate) struct SemanticAdmission {
    permits: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
    wait_timeout: Duration,
}

impl SemanticAdmission {
    pub(crate) fn new(permits: NonZeroUsize, waiter_capacity: NonZeroUsize, wait_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(permits.get())),
            waiters: Arc::new(Semaphore::new(waiter_capacity.get())),
            wait_timeout,
        })
    }

    async fn acquire(self: &Arc<Self>) -> std::result::Result<OwnedSemaphorePermit, AdmissionError> {
        let waiter = Arc::clone(&self.waiters).try_acquire_owned().map_err(|err| match err {
            tokio::sync::TryAcquireError::NoPermits => AdmissionError::Full,
            tokio::sync::TryAcquireError::Closed => AdmissionError::Closed,
        })?;
        let acquire = Arc::clone(&self.permits).acquire_owned();
        let permit = tokio::time::timeout(self.wait_timeout, acquire)
            .await
            .map_err(|_| AdmissionError::Timeout)?
            .map_err(|_| AdmissionError::Closed)?;
        drop(waiter);
        Ok(permit)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionError {
    Full,
    Timeout,
    Closed,
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    worker_generation: u64,
    restart_reason: Option<String>,
}

impl RuntimeSnapshot {
    fn facts(&self) -> RuntimeFacts {
        RuntimeFacts {
            worker_generation: self.worker_generation,
            worker_restarted: false,
            retry_count: 0,
            admission_wait_millis: 0,
            queue_wait_millis: 0,
            restart_reason: self.restart_reason.clone(),
        }
    }
}

/// One Lake project, one supervised worker actor, one in-memory cache. Cheap
/// to clone via `Arc`.
pub(crate) struct LeanProject {
    canonical_root: PathBuf,
    toolchain: String,
    package: Option<String>,
    library: Option<String>,
    manifest_hash: String,
    session_id: String,
    actor_tx: Mutex<Option<mpsc::Sender<ProjectMessage>>>,
    admission: Arc<SemanticAdmission>,
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
    pub(crate) fn open_with_admission(meta: LakeProjectMeta, admission: Arc<SemanticAdmission>) -> Result<Arc<Self>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            restart_reason: None,
        }));
        let active_jobs = Arc::new(AtomicUsize::new(0));
        let healthy = Arc::new(AtomicBool::new(true));
        let config = ActorConfig::from_meta(&meta, session_id.clone(), Arc::clone(&runtime), Arc::clone(&healthy))?;
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
            actor_tx: Mutex::new(Some(actor_tx)),
            admission,
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
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQuery {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
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
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerModuleQueryBatchOutcome>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ModuleQueryBatch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
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
        request: LeanWorkerDeclarationInspectionRequest,
    ) -> Result<ProjectCall<LeanWorkerDeclarationInspectionResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationInspection {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
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
        request: LeanWorkerDeclarationSearch,
    ) -> Result<ProjectCall<LeanWorkerDeclarationSearchResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationSearch {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
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
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerProofAttemptResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::ProofAttempt {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
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
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<ProjectCall<LeanWorkerDeclarationVerificationResult>> {
        let (reply, rx) = oneshot::channel();
        let message = ProjectMessage::DeclarationVerification {
            meta: self.job_meta(imports, RetryPolicy::RetryOnceReadOnly).await?,
            request,
            options,
            reply,
        };
        self.enqueue(message, rx).await
    }

    async fn job_meta(&self, imports: Vec<String>, retry_policy: RetryPolicy) -> Result<JobMeta> {
        let created_at = Instant::now();
        let semantic_permit = self.admission.acquire().await.map_err(|err| {
            let reason = match err {
                AdmissionError::Full => "semantic_admission_full",
                AdmissionError::Timeout => "semantic_admission_timeout",
                AdmissionError::Closed => "semantic_admission_closed",
            };
            ServerError::WorkerUnavailable(WorkerUnavailable {
                retryable: !matches!(err, AdmissionError::Closed),
                worker_restarted: false,
                reason: reason.to_owned(),
                ..self.worker_error_context()
            })
        })?;
        let admission_wait_millis = millis_u64(created_at.elapsed());
        self.active_jobs.fetch_add(1, Ordering::AcqRel);
        Ok(JobMeta {
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
        })
    }

    async fn enqueue<T>(
        &self,
        message: ProjectMessage,
        reply_rx: oneshot::Receiver<Result<ProjectCall<T>>>,
    ) -> Result<ProjectCall<T>>
    where
        T: Send + 'static,
    {
        let project_info = self.worker_error_context();
        let tx = self
            .actor_tx
            .lock()
            .as_ref()
            .cloned()
            .ok_or_else(|| self.unavailable("project actor is stopped", false, false))?;
        match tx.try_send(message) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Err(ServerError::WorkerUnavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "mailbox_full".to_owned(),
                    ..project_info
                }));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.shutdown();
                return Err(ServerError::WorkerUnavailable(WorkerUnavailable {
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
        ServerError::WorkerUnavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            reason: reason.into(),
            ..self.worker_error_context()
        })
    }

    fn worker_error_context(&self) -> WorkerUnavailable {
        let snapshot = self.runtime.lock().clone();
        WorkerUnavailable {
            retryable: true,
            worker_restarted: false,
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            session_id: self.session_id.clone(),
            worker_generation: snapshot.worker_generation,
            reason: String::new(),
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
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    session_id: String,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    healthy: Arc<AtomicBool>,
    worker_rss_ceiling_kib: u64,
    module_cache_rss_guard_kib: u64,
    module_cache_max_bytes: u64,
    mailbox_capacity: usize,
    max_restarts_per_window: usize,
    restart_window: Duration,
}

impl ActorConfig {
    fn from_meta(
        meta: &LakeProjectMeta,
        session_id: String,
        runtime: Arc<Mutex<RuntimeSnapshot>>,
        healthy: Arc<AtomicBool>,
    ) -> Result<Self> {
        let toolchain_id = ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let worker = WorkerBinary::resolve_for(&toolchain_id).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let lean_sysroot = toolchain_id
            .elan_dir()
            .map_err(|e| ServerError::BadProject(e.to_string()))?;
        Ok(Self {
            lake_root: meta.canonical_root.clone(),
            toolchain_label: meta.toolchain.clone(),
            worker_path: worker.path,
            lean_sysroot,
            session_id,
            runtime,
            healthy,
            worker_rss_ceiling_kib: env_u64("LEAN_HOST_MCP_WORKER_RSS_CEILING_KIB", WORKER_RSS_CEILING_KIB)?,
            module_cache_rss_guard_kib: env_u64(
                "LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB",
                MODULE_CACHE_RSS_GUARD_KIB,
            )?,
            module_cache_max_bytes: env_u64("LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES", MODULE_CACHE_MAX_BYTES)?,
            mailbox_capacity: env_usize("LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY", PROJECT_MAILBOX_CAPACITY)?,
            max_restarts_per_window: env_usize("LEAN_HOST_MCP_WORKER_RESTART_LIMIT", MAX_RESTARTS_PER_WINDOW)?,
            restart_window: Duration::from_secs(env_u64(
                "LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS",
                RESTART_WINDOW.as_secs(),
            )?),
        })
    }
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    worker_generation_base: u64,
    last_restart_reason: Option<String>,
    last_import_fingerprint: Option<String>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    restart_times: VecDeque<Instant>,
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
        }
    }

    fn run_job<R>(
        &mut self,
        meta: JobMeta,
        job: impl Fn(&mut LeanWorkerHostHandle, Vec<String>) -> std::result::Result<R, LeanWorkerError>,
    ) -> Result<ProjectCall<R>> {
        let queue_wait_millis = millis_u64(meta.queued_at.elapsed());
        let generation_before = self.observed_generation();
        self.cycle_before_import_switch_if_needed(&meta.import_fingerprint)?;
        let mut lifecycle_baseline = self.handle.lifecycle_snapshot();

        let max_retries = meta.retry_policy.retries();
        let mut retry_count = 0_u32;
        loop {
            match job(&mut self.handle, meta.imports.clone()) {
                Ok(value) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    self.last_import_fingerprint = Some(meta.import_fingerprint.clone());
                    let runtime = self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis);
                    self.publish_runtime(&runtime);
                    return Ok(ProjectCall { value, runtime });
                }
                Err(err) if worker_error_is_recoverable_death(&err) && retry_count < max_retries => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    let first_reason = err.to_string();
                    self.rebuild_after_worker_death(first_reason)?;
                    lifecycle_baseline = self.handle.lifecycle_snapshot();
                    retry_count = retry_count.saturating_add(1);
                }
                Err(err) if worker_error_is_recoverable_death(&err) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    let reason = format!("worker_died_after_retry: {err}");
                    let generation = self.observed_generation();
                    let runtime = self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable(reason, true, generation > generation_before));
                }
                Err(err) if worker_error_is_session_missing(&err) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    let generation = self.observed_generation();
                    let runtime = self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable(
                        format!("session_missing: {err}"),
                        true,
                        generation > generation_before,
                    ));
                }
                Err(err) if matches!(err, LeanWorkerError::Timeout { .. }) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    let generation = self.observed_generation();
                    let runtime = self.runtime_facts(&meta, generation_before, retry_count, queue_wait_millis);
                    self.publish_runtime(&runtime);
                    return Err(self.worker_unavailable(
                        format!("timeout: {err}"),
                        true,
                        generation > generation_before,
                    ));
                }
                Err(err) => {
                    self.account_lifecycle_restarts_since(&lifecycle_baseline)?;
                    self.last_import_fingerprint = Some(meta.import_fingerprint.clone());
                    return Err(map_worker_err(err));
                }
            }
        }
    }

    fn cycle_before_import_switch_if_needed(&mut self, import_fingerprint: &str) -> Result<()> {
        if self.last_import_fingerprint.as_deref() == Some(import_fingerprint) {
            return Ok(());
        }
        let Some(current_kib) = self.handle.rss_kib() else {
            return Ok(());
        };
        if current_kib < self.config.worker_rss_ceiling_kib {
            return Ok(());
        }
        let reason = format!(
            "rss_import_switch current_kib={current_kib} limit_kib={}",
            self.config.worker_rss_ceiling_kib
        );
        self.record_restart_or_stop(&reason)?;
        self.handle.restart().map_err(map_worker_err)?;
        self.last_restart_reason = Some(reason);
        Ok(())
    }

    fn rebuild_after_worker_death(&mut self, reason: String) -> Result<()> {
        self.record_restart_or_stop(&reason)?;
        let next_generation = self.observed_generation().saturating_add(1);
        let (handle, _) = open_worker(&self.config, false)?;
        self.handle = handle;
        self.worker_generation_base = next_generation;
        self.last_restart_reason = Some(reason);
        Ok(())
    }

    fn account_lifecycle_restarts_since(&mut self, before: &LeanWorkerLifecycleSnapshot) -> Result<()> {
        let after = self.handle.lifecycle_snapshot();
        let restarted = after.restarts.saturating_sub(before.restarts);
        if restarted == 0 {
            return Ok(());
        }
        let reason = after
            .last_restart_reason
            .as_ref()
            .map_or_else(|| "worker_internal_restart".to_owned(), |reason| format!("{reason:?}"));
        for _ in 0..restarted {
            self.record_restart_or_stop(&reason)?;
        }
        self.last_restart_reason = Some(reason);
        Ok(())
    }

    fn record_restart_or_stop(&mut self, reason: &str) -> Result<()> {
        let now = Instant::now();
        while self
            .restart_times
            .front()
            .is_some_and(|seen| now.saturating_duration_since(*seen) > self.config.restart_window)
        {
            self.restart_times.pop_front();
        }
        if self.restart_times.len() >= self.config.max_restarts_per_window {
            self.config.healthy.store(false, Ordering::Release);
            let message = format!(
                "restart_limit_exceeded after {} restarts in {:?}; latest: {reason}",
                self.config.max_restarts_per_window, self.config.restart_window
            );
            self.last_restart_reason = Some(message.clone());
            self.publish_runtime(&RuntimeFacts {
                worker_generation: self.observed_generation(),
                worker_restarted: false,
                retry_count: MAX_JOB_RETRIES,
                admission_wait_millis: 0,
                queue_wait_millis: 0,
                restart_reason: Some(message.clone()),
            });
            return Err(self.worker_unavailable(message, false, false));
        }
        self.restart_times.push_back(now);
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
    ) -> RuntimeFacts {
        let generation = self.observed_generation();
        let restart_reason = self
            .handle
            .lifecycle_snapshot()
            .last_restart_reason
            .as_ref()
            .map(|reason| format!("{reason:?}"))
            .or_else(|| self.last_restart_reason.clone());
        RuntimeFacts {
            worker_generation: generation,
            worker_restarted: generation > generation_before,
            retry_count,
            admission_wait_millis: meta.admission_wait_millis,
            queue_wait_millis,
            restart_reason,
        }
    }

    fn publish_runtime(&self, runtime: &RuntimeFacts) {
        *self.runtime.lock() = RuntimeSnapshot {
            worker_generation: runtime.worker_generation,
            restart_reason: runtime.restart_reason.clone(),
        };
    }

    fn worker_unavailable(&self, reason: String, retryable: bool, worker_restarted: bool) -> ServerError {
        ServerError::WorkerUnavailable(WorkerUnavailable {
            retryable,
            worker_restarted,
            project_root: self.config.lake_root.to_string_lossy().into_owned(),
            session_id: self.config.session_id.clone(),
            worker_generation: self.observed_generation(),
            reason,
        })
    }
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
        last_restart_reason: None,
        last_import_fingerprint: None,
        runtime: Arc::clone(&config.runtime),
        restart_times: VecDeque::new(),
    };

    let (tx, mut rx) = mpsc::channel::<ProjectMessage>(config.mailbox_capacity);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    while let Some(message) = rx.blocking_recv() {
        state.handle_message(message);
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
    let handle = builder.open().map_err(map_worker_err)?;
    let runtime_toolchain = handle
        .runtime_metadata()
        .lean_version
        .unwrap_or_else(|| config.toolchain_label.clone());
    Ok((handle, runtime_toolchain))
}

fn worker_builder(config: &ActorConfig) -> LeanWorkerHostHandleBuilder {
    let restart_policy = LeanWorkerRestartPolicy::default().max_requests(WORKER_REQUEST_RESTARTS);
    let module_cache_limits = LeanWorkerModuleCacheLimits::default()
        .rss_guard_kib(config.module_cache_rss_guard_kib)
        .max_bytes(config.module_cache_max_bytes);
    LeanWorkerHostHandleBuilder::shims_only(&config.lake_root, std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(
            config.worker_path.clone(),
            config.lean_sysroot.clone(),
        ))
        .startup_timeout(Duration::from_secs(30))
        .long_running_requests()
        .restart_policy(restart_policy)
        .module_cache_limits(module_cache_limits)
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

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    match std::env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<u64>()
                .map_err(|e| ServerError::Internal(format!("{name}={value:?} not a u64: {e}")))?;
            if parsed == 0 {
                return Err(ServerError::Internal(format!("{name}=0 is not allowed")));
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(ServerError::Internal(format!("{name} is not valid unicode: {e}"))),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize> {
    let value = env_u64(name, default as u64)?;
    usize::try_from(value).map_err(|_| ServerError::Internal(format!("{name}={value} does not fit in usize")))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests use expect/unwrap_err to state the branch under test directly"
)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn semantic_admission_bounds_waiters() {
        let admission = SemanticAdmission::new(NonZeroUsize::MIN, NonZeroUsize::MIN, Duration::from_secs(5));
        let held = admission.acquire().await.expect("initial permit");
        let waiting_admission = Arc::clone(&admission);
        let waiting = tokio::spawn(async move { waiting_admission.acquire().await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(admission.acquire().await.unwrap_err(), AdmissionError::Full);

        drop(held);
        drop(waiting.await.expect("waiter task").expect("waiter permit"));
    }

    #[tokio::test]
    async fn semantic_admission_times_out() {
        let admission = SemanticAdmission::new(NonZeroUsize::MIN, NonZeroUsize::MIN, Duration::from_millis(10));
        let _held = admission.acquire().await.expect("initial permit");

        assert_eq!(admission.acquire().await.unwrap_err(), AdmissionError::Timeout);
    }
}
