//! `LeanProject`—the unit of Lean semantic execution.
//!
//! One Lake project owns one private actor. The actor serializes all semantic
//! worker calls, owns the child-process supervisor, applies memory/restart
//! policy, and exposes only typed request/reply calls to tool modules. Worker
//! handles, channels, queue internals, and restart mechanics stay below this
//! boundary.

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerError, LeanWorkerHostHandle, LeanWorkerHostHandleBuilder, LeanWorkerModuleCacheLimits,
    LeanWorkerRestartPolicy,
};
use parking_lot::{Condvar, Mutex};
use tokio::sync::{mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError, WorkerUnavailable};
use crate::lake_meta::LakeProjectMeta;
use crate::projections::map_worker_err;
use crate::toolchain::{ToolchainId, WorkerBinary};

/// LRU capacity for exact bounded module query results.
const MODULE_QUERY_CACHE_CAPACITY: usize = 256;
const WORKER_REQUEST_RESTARTS: u64 = 64;
const PROJECT_MAILBOX_CAPACITY: usize = 8;
const WORKER_RSS_CEILING_KIB: u64 = 3 * 1024 * 1024;
const MODULE_CACHE_RSS_GUARD_KIB: u64 = 2 * 1024 * 1024;
const MODULE_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_JOB_RETRIES: u32 = 1;

type ActorMessage = Box<dyn FnOnce(&mut ProjectActorState) + Send + 'static>;

/// Coarse work class for project-actor scheduling and observability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectWorkClass {
    Semantic,
}

/// Result of one project actor call.
#[derive(Debug, Clone)]
pub struct ProjectCall<T> {
    pub value: T,
    pub runtime: RuntimeFacts,
}

/// Process-wide permit gate for heavy Lean semantic work.
#[derive(Debug)]
pub struct SemanticGate {
    state: Mutex<SemanticGateState>,
    wake: Condvar,
}

#[derive(Debug)]
struct SemanticGateState {
    available: usize,
}

impl SemanticGate {
    pub(crate) fn new(permits: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SemanticGateState {
                available: permits.get(),
            }),
            wake: Condvar::new(),
        })
    }

    fn acquire(self: &Arc<Self>) -> SemanticPermit {
        let mut state = self.state.lock();
        while state.available == 0 {
            self.wake.wait(&mut state);
        }
        state.available = state.available.saturating_sub(1);
        drop(state);
        SemanticPermit { gate: Arc::clone(self) }
    }

    fn release(&self) {
        let mut state = self.state.lock();
        state.available = state.available.saturating_add(1);
        drop(state);
        self.wake.notify_one();
    }
}

struct SemanticPermit {
    gate: Arc<SemanticGate>,
}

impl Drop for SemanticPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
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
            queue_wait_millis: 0,
            restart_reason: self.restart_reason.clone(),
        }
    }
}

/// One Lake project, one supervised worker actor, one in-memory cache. Cheap
/// to clone via `Arc`.
pub struct LeanProject {
    canonical_root: PathBuf,
    toolchain: String,
    package: Option<String>,
    library: Option<String>,
    manifest_hash: String,
    session_id: String,
    actor_tx: Mutex<Option<mpsc::Sender<ActorMessage>>>,
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
    /// Spawn the per-project worker actor and return a shareable handle.
    ///
    /// # Errors
    ///
    /// `ServerError::BadProject` for unresolvable worker child / failing
    /// shims-only bootstrap / handshake failure; `ServerError::Lean` for
    /// runtime open failures; `ServerError::Internal` if the OS rejects the
    /// actor thread.
    pub fn open(meta: LakeProjectMeta, cache_dir: &Path) -> Result<Arc<Self>> {
        Self::open_with_gate(meta, cache_dir, SemanticGate::new(NonZeroUsize::MIN))
    }

    pub(crate) fn open_with_gate(
        meta: LakeProjectMeta,
        _cache_dir: &Path,
        semantic_gate: Arc<SemanticGate>,
    ) -> Result<Arc<Self>> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let runtime = Arc::new(Mutex::new(RuntimeSnapshot {
            worker_generation: 1,
            restart_reason: None,
        }));
        let config = ActorConfig::from_meta(&meta, semantic_gate, session_id.clone(), Arc::clone(&runtime))?;
        type InitMsg = std::result::Result<(String, mpsc::Sender<ActorMessage>), ServerError>;
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
            runtime,
            module_queries: ModuleQueryCache::with_capacity(cache_cap),
        }))
    }

    /// Dispatch one retryable read-only semantic job to this project's actor.
    ///
    /// The job may be called twice when the first attempt loses the worker
    /// child. It must therefore be idempotent and must not mutate user files.
    ///
    /// # Errors
    ///
    /// `ServerError::WorkerUnavailable` when the mailbox is full, the actor is
    /// gone, or worker restart recovery fails.
    pub async fn call<F, R>(&self, work_class: ProjectWorkClass, imports: Vec<String>, job: F) -> Result<ProjectCall<R>>
    where
        F: Fn(&mut LeanWorkerHostHandle) -> std::result::Result<R, LeanWorkerError> + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let queued_at = Instant::now();
        let import_fingerprint = import_fingerprint(&imports);
        let project_info = self.worker_error_context();
        let boxed_job: Box<dyn Fn(&mut LeanWorkerHostHandle) -> std::result::Result<R, LeanWorkerError> + Send> =
            Box::new(job);
        let message: ActorMessage = Box::new(move |state| {
            let result = state.run_job(work_class, import_fingerprint, queued_at, &*boxed_job);
            let _ = reply_tx.send(result);
        });

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
                    reason: "project worker mailbox is full".to_owned(),
                    ..project_info
                }));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.shutdown();
                return Err(ServerError::WorkerUnavailable(WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    reason: "project worker mailbox is closed".to_owned(),
                    ..project_info
                }));
            }
        }

        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.shutdown();
                Err(self.unavailable("project actor stopped before replying", true, false))
            }
        }
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn package(&self) -> Option<&str> {
        self.package.as_deref()
    }

    pub fn library(&self) -> Option<&str> {
        self.library.as_deref()
    }

    pub fn toolchain(&self) -> &str {
        &self.toolchain
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn module_query_cache(&self) -> &ModuleQueryCache {
        &self.module_queries
    }

    #[must_use]
    pub fn freshness(&self, request_imports: &[String]) -> Freshness {
        Freshness {
            project_root: self.canonical_root.to_string_lossy().into_owned(),
            project_hash: self.manifest_hash.clone(),
            imports: request_imports.to_vec(),
            session_id: self.session_id.clone(),
            lean_toolchain: self.toolchain.clone(),
        }
    }

    #[must_use]
    pub fn runtime_facts(&self) -> RuntimeFacts {
        self.runtime.lock().facts()
    }

    pub fn shutdown(&self) {
        let _ = self.actor_tx.lock().take();
    }

    pub fn is_healthy(&self) -> bool {
        self.actor_tx.lock().as_ref().is_some_and(|tx| !tx.is_closed())
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
    semantic_gate: Arc<SemanticGate>,
    session_id: String,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
    worker_rss_ceiling_kib: u64,
    module_cache_rss_guard_kib: u64,
    module_cache_max_bytes: u64,
    mailbox_capacity: usize,
}

impl ActorConfig {
    fn from_meta(
        meta: &LakeProjectMeta,
        semantic_gate: Arc<SemanticGate>,
        session_id: String,
        runtime: Arc<Mutex<RuntimeSnapshot>>,
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
            semantic_gate,
            session_id,
            runtime,
            worker_rss_ceiling_kib: env_u64("LEAN_HOST_MCP_WORKER_RSS_CEILING_KIB", WORKER_RSS_CEILING_KIB),
            module_cache_rss_guard_kib: env_u64("LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB", MODULE_CACHE_RSS_GUARD_KIB),
            module_cache_max_bytes: env_u64("LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES", MODULE_CACHE_MAX_BYTES),
            mailbox_capacity: usize::try_from(env_u64(
                "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
                PROJECT_MAILBOX_CAPACITY as u64,
            ))
            .unwrap_or(PROJECT_MAILBOX_CAPACITY)
            .max(1),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorPhase {
    Ready,
    Restarting,
    Draining,
    Stopped,
}

struct ProjectActorState {
    config: ActorConfig,
    handle: LeanWorkerHostHandle,
    phase: ActorPhase,
    worker_generation_base: u64,
    last_restart_reason: Option<String>,
    last_import_fingerprint: Option<String>,
    runtime: Arc<Mutex<RuntimeSnapshot>>,
}

impl ProjectActorState {
    fn run_job<R>(
        &mut self,
        _work_class: ProjectWorkClass,
        import_fingerprint: String,
        queued_at: Instant,
        job: &(dyn Fn(&mut LeanWorkerHostHandle) -> std::result::Result<R, LeanWorkerError> + Send),
    ) -> Result<ProjectCall<R>> {
        self.phase = ActorPhase::Ready;
        let queue_wait_millis = millis_u64(queued_at.elapsed());
        let _permit = self.config.semantic_gate.acquire();
        let generation_before = self.observed_generation();
        self.cycle_before_import_switch_if_needed(&import_fingerprint)?;

        match job(&mut self.handle) {
            Ok(value) => {
                self.last_import_fingerprint = Some(import_fingerprint);
                let runtime = self.runtime_facts(generation_before, 0, queue_wait_millis);
                self.publish_runtime(&runtime);
                Ok(ProjectCall { value, runtime })
            }
            Err(err) if worker_error_is_recoverable_death(&err) => {
                let first_reason = err.to_string();
                self.rebuild_after_worker_death(first_reason.clone())?;
                match job(&mut self.handle) {
                    Ok(value) => {
                        self.last_import_fingerprint = Some(import_fingerprint);
                        let runtime = self.runtime_facts(generation_before, 1, queue_wait_millis);
                        self.publish_runtime(&runtime);
                        Ok(ProjectCall { value, runtime })
                    }
                    Err(second) if worker_error_is_recoverable_death(&second) => {
                        let reason =
                            format!("restart_limit_exceeded after worker death: {first_reason}; retry: {second}");
                        let generation = self.observed_generation();
                        self.last_restart_reason = Some(reason.clone());
                        self.publish_runtime(&RuntimeFacts {
                            worker_generation: generation,
                            worker_restarted: generation > generation_before,
                            retry_count: MAX_JOB_RETRIES,
                            queue_wait_millis,
                            restart_reason: Some(reason.clone()),
                        });
                        Err(self.worker_unavailable(reason, false, generation > generation_before))
                    }
                    Err(second) => Err(map_worker_err(second)),
                }
            }
            Err(err) => {
                self.last_import_fingerprint = Some(import_fingerprint);
                Err(map_worker_err(err))
            }
        }
    }

    fn cycle_before_import_switch_if_needed(&mut self, import_fingerprint: &str) -> Result<()> {
        if self.last_import_fingerprint.as_deref() == Some(import_fingerprint) {
            return Ok(());
        }
        let Some(current_kib) = self.handle.worker_mut().rss_kib() else {
            return Ok(());
        };
        if current_kib < self.config.worker_rss_ceiling_kib {
            return Ok(());
        }
        let reason = format!(
            "rss_import_switch current_kib={current_kib} limit_kib={}",
            self.config.worker_rss_ceiling_kib
        );
        self.phase = ActorPhase::Restarting;
        self.handle.worker_mut().restart().map_err(map_worker_err)?;
        self.last_restart_reason = Some(reason);
        self.phase = ActorPhase::Ready;
        Ok(())
    }

    fn rebuild_after_worker_death(&mut self, reason: String) -> Result<()> {
        self.phase = ActorPhase::Restarting;
        let next_generation = self.observed_generation().saturating_add(1);
        let (handle, _) = open_worker(&self.config, false)?;
        self.handle = handle;
        self.worker_generation_base = next_generation;
        self.last_restart_reason = Some(reason);
        self.phase = ActorPhase::Ready;
        Ok(())
    }

    fn observed_generation(&self) -> u64 {
        self.worker_generation_base
            .saturating_add(self.handle.worker().stats().restarts)
    }

    fn runtime_facts(&self, generation_before: u64, retry_count: u32, queue_wait_millis: u64) -> RuntimeFacts {
        let generation = self.observed_generation();
        let restart_reason = self
            .handle
            .worker()
            .stats()
            .last_restart_reason
            .as_ref()
            .map(|reason| format!("{reason:?}"))
            .or_else(|| self.last_restart_reason.clone());
        RuntimeFacts {
            worker_generation: generation,
            worker_restarted: generation > generation_before,
            retry_count,
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
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<ActorMessage>), ServerError>>,
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
        phase: ActorPhase::Ready,
        worker_generation_base: 1,
        last_restart_reason: None,
        last_import_fingerprint: None,
        runtime: Arc::clone(&config.runtime),
    };

    let (tx, mut rx) = mpsc::channel::<ActorMessage>(config.mailbox_capacity);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        state.phase = ActorPhase::Stopped;
        return;
    }

    while let Some(job) = rx.blocking_recv() {
        job(&mut state);
    }
    state.phase = ActorPhase::Draining;
    state.phase = ActorPhase::Stopped;
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
    let restart_policy = LeanWorkerRestartPolicy::default()
        .max_requests(WORKER_REQUEST_RESTARTS)
        .max_rss_kib(config.worker_rss_ceiling_kib);
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
    ) || matches!(
        err,
        LeanWorkerError::Worker { code, .. } if code == "lean_rs.worker.session_missing"
    )
}

fn import_fingerprint(imports: &[String]) -> String {
    imports.join("\n")
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}
