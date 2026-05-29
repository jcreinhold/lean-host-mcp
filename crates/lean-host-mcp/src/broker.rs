//! `ProjectBroker`: the mediator between MCP tool dispatch and
//! the private per-project Lean runtime.
//!
//! Two responsibilities:
//!
//! 1. **Resolve** a tool call's [`ProjectHint`] into a canonical Lake-root
//!    path via the five-step chain
//!    *explicit → env → cwd-walk → config-default → error*.
//! 2. **Dispatch** typed semantic operations to a private per-project
//!    runtime, opening the project lazily on first use, reusing it on
//!    subsequent calls, and evicting under LRU and idle pressure.
//!
//! Tool modules call narrow domain methods such as
//! [`ProjectBroker::inspect_declaration`] and never receive raw project actor
//! handles.
//!
//! **LRU + idle eviction.** The registry is an [`lru::LruCache`] capped by
//! [`BrokerConfig::max_projects`] (default 4). A background reaper spawned
//! from [`ProjectBroker::new`] runs every 60 s and evicts entries that have
//! been idle longer than [`BrokerConfig::idle_timeout`] (default 600 s; set
//! to [`Duration::ZERO`] to disable idle eviction).
//!
//! **Manifest invalidation.** Every cache hit re-fingerprints
//! `lake-manifest.json` and treats a mismatch as a miss: the project is
//! shut down and re-spawned. The cost is one ≤ 50 KB SHA-256 per tool call.
//!
//! **Slow-path concurrency.** The registry mutex is never held across
//! worker spawn or command execution. Concurrent misses for the same
//! canonical root are coalesced so only one project actor is opened. Heavy semantic jobs are
//! additionally gated by a process-wide permit owned by the broker.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationInspectionRequest, LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationSearch,
    LeanWorkerDeclarationSearchResult, LeanWorkerDeclarationVerificationRequest,
    LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions, LeanWorkerModuleQuery,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector,
    LeanWorkerOutputBudgets, LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult,
};
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::cache::{ModuleQueryBatchKey, ModuleQueryKey};
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError};
use crate::index::fingerprint_lake_project;
use crate::lake_meta::LakeProjectMeta;
use crate::project::{LeanProject, ProjectCall, SemanticAdmission};

/// Default pool capacity when `LEAN_HOST_MCP_MAX_PROJECTS` is unset.
pub const DEFAULT_MAX_PROJECTS: usize = 4;

/// Default idle-eviction window (10 min) when
/// `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` is unset.
///
/// Long enough that an interactive Claude session keeps its projects warm,
/// short enough that a forgotten project tab releases its worker child
/// within a reasonable bound.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_SEMANTIC_PERMITS: usize = 1;
pub const DEFAULT_SEMANTIC_WAITERS: usize = 16;
pub const DEFAULT_SEMANTIC_ADMISSION_TIMEOUT_MILLIS: u64 = 60_000;

const REAPER_TICK: Duration = Duration::from_mins(1);

/// Bag of broker inputs. Built once at startup from the CLI / env /
/// config; `cwd` is injectable so tests can drive the cwd-walk step
/// without `std::env::set_current_dir`.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub config_default: Option<PathBuf>,
    pub env_default: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Maximum number of private project runtimes kept resident. A miss
    /// when the pool is full evicts the LRU entry.
    pub max_projects: NonZeroUsize,
    /// Idle window after which a project is eligible for the reaper.
    /// [`Duration::ZERO`] disables idle eviction (LRU only).
    pub idle_timeout: Duration,
    /// Process-wide permits for heavy Lean semantic work.
    pub semantic_permits: NonZeroUsize,
    /// Process-wide capacity for callers waiting on semantic-work permits.
    pub semantic_waiters: NonZeroUsize,
    /// Maximum time a caller may wait for semantic-work admission.
    pub semantic_admission_timeout: Duration,
}

impl BrokerConfig {
    /// Parse the two pool-related env vars and return their values, falling
    /// back to [`DEFAULT_MAX_PROJECTS`] / [`DEFAULT_IDLE_TIMEOUT_SECS`].
    ///
    /// `LEAN_HOST_MCP_MAX_PROJECTS = 0` is rejected (it would deadlock the
    /// pool); `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS = 0` disables idle eviction.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is set but unparseable
    /// (non-numeric, or `MAX_PROJECTS=0`).
    pub fn pool_from_env() -> Result<(NonZeroUsize, Duration, NonZeroUsize, NonZeroUsize, Duration)> {
        parse_pool_config(
            std::env::var("LEAN_HOST_MCP_MAX_PROJECTS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_SEMANTIC_PERMITS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_SEMANTIC_WAITERS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS")
                .ok()
                .as_deref(),
        )
    }

    /// Convenience: a [`NonZeroUsize`] with the default pool capacity. Used
    /// by tests, benches, and any caller that just wants the default pool
    /// sizing without rederiving it from [`DEFAULT_MAX_PROJECTS`].
    #[must_use]
    pub fn default_max_projects() -> NonZeroUsize {
        NonZeroUsize::new(DEFAULT_MAX_PROJECTS).unwrap_or(NonZeroUsize::MIN)
    }

    /// Convenience: the default idle window as a [`Duration`].
    #[must_use]
    pub const fn default_idle_timeout() -> Duration {
        Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
    }

    /// Convenience: default global semantic-work permit count.
    #[must_use]
    pub fn default_semantic_permits() -> NonZeroUsize {
        NonZeroUsize::new(DEFAULT_SEMANTIC_PERMITS).unwrap_or(NonZeroUsize::MIN)
    }

    /// Convenience: default global semantic-admission waiter capacity.
    #[must_use]
    pub fn default_semantic_waiters() -> NonZeroUsize {
        NonZeroUsize::new(DEFAULT_SEMANTIC_WAITERS).unwrap_or(NonZeroUsize::MIN)
    }

    /// Convenience: default global semantic-admission wait timeout.
    #[must_use]
    pub const fn default_semantic_admission_timeout() -> Duration {
        Duration::from_millis(DEFAULT_SEMANTIC_ADMISSION_TIMEOUT_MILLIS)
    }
}

/// Pure parser shared by [`BrokerConfig::pool_from_env`] and unit tests.
fn parse_pool_config(
    max: Option<&str>,
    idle: Option<&str>,
    semantic: Option<&str>,
    semantic_waiters: Option<&str>,
    semantic_timeout_millis: Option<&str>,
) -> Result<(NonZeroUsize, Duration, NonZeroUsize, NonZeroUsize, Duration)> {
    let max_projects = match max {
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|e| ServerError::Internal(format!("LEAN_HOST_MCP_MAX_PROJECTS={s:?} not a usize: {e}")))?;
            NonZeroUsize::new(n)
                .ok_or_else(|| ServerError::Internal("LEAN_HOST_MCP_MAX_PROJECTS=0 would deadlock the pool".into()))?
        }
        None => BrokerConfig::default_max_projects(),
    };
    let idle_timeout = match idle {
        Some(s) => {
            let n: u64 = s
                .parse()
                .map_err(|e| ServerError::Internal(format!("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS={s:?} not a u64: {e}")))?;
            Duration::from_secs(n)
        }
        None => BrokerConfig::default_idle_timeout(),
    };
    let semantic_permits = match semantic {
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|e| ServerError::Internal(format!("LEAN_HOST_MCP_SEMANTIC_PERMITS={s:?} not a usize: {e}")))?;
            NonZeroUsize::new(n).ok_or_else(|| {
                ServerError::Internal("LEAN_HOST_MCP_SEMANTIC_PERMITS=0 would deadlock semantic work".into())
            })?
        }
        None => BrokerConfig::default_semantic_permits(),
    };
    let semantic_waiters = match semantic_waiters {
        Some(s) => {
            let n: usize = s
                .parse()
                .map_err(|e| ServerError::Internal(format!("LEAN_HOST_MCP_SEMANTIC_WAITERS={s:?} not a usize: {e}")))?;
            NonZeroUsize::new(n).ok_or_else(|| {
                ServerError::Internal("LEAN_HOST_MCP_SEMANTIC_WAITERS=0 would reject all waiters".into())
            })?
        }
        None => BrokerConfig::default_semantic_waiters(),
    };
    let semantic_admission_timeout = match semantic_timeout_millis {
        Some(s) => {
            let n: u64 = s.parse().map_err(|e| {
                ServerError::Internal(format!(
                    "LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS={s:?} not a u64: {e}"
                ))
            })?;
            if n == 0 {
                return Err(ServerError::Internal(
                    "LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS=0 is not allowed".into(),
                ));
            }
            Duration::from_millis(n)
        }
        None => BrokerConfig::default_semantic_admission_timeout(),
    };
    Ok((
        max_projects,
        idle_timeout,
        semantic_permits,
        semantic_waiters,
        semantic_admission_timeout,
    ))
}

/// Per-call routing hint. `Default` runs the full resolution chain;
/// `Explicit` short-circuits to the supplied path.
#[derive(Debug, Clone)]
pub enum ProjectHint {
    Explicit(PathBuf),
    Default,
}

impl ProjectHint {
    /// Build a hint from a tool request's optional `project` field.
    /// `None`/empty string falls through to [`Self::Default`].
    #[must_use]
    pub fn from_request(value: Option<String>) -> Self {
        match value {
            Some(s) if !s.is_empty() => Self::Explicit(PathBuf::from(s)),
            _ => Self::Default,
        }
    }
}

struct BrokerInner {
    registry: LruCache<PathBuf, Arc<LeanProject>>,
    last_used: HashMap<PathBuf, Instant>,
    opening_locks: HashMap<PathBuf, Arc<AsyncMutex<()>>>,
}

#[derive(Debug, Clone)]
pub struct BrokerCall<T> {
    pub value: T,
    pub runtime: RuntimeFacts,
    pub freshness: Freshness,
}

#[derive(Debug, Clone)]
pub(crate) struct CachedBrokerCall<T> {
    pub value: T,
    pub runtime: RuntimeFacts,
    pub freshness: Freshness,
    pub freshly_processed: bool,
}

pub struct ProjectBroker {
    inner: Mutex<BrokerInner>,
    config: BrokerConfig,
    semantic_admission: Arc<SemanticAdmission>,
}

impl std::fmt::Debug for ProjectBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectBroker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProjectBroker {
    /// Construct a broker and spawn the background idle reaper.
    ///
    /// The reaper is only spawned when a tokio runtime is available
    /// ([`tokio::runtime::Handle::try_current`]) and `idle_timeout > 0`.
    /// Sync test contexts get a working broker without a reaper, which is
    /// fine because tests call [`Self::reap_idle`] directly.
    #[must_use]
    pub fn new(config: BrokerConfig) -> Arc<Self> {
        let broker = Arc::new(Self {
            inner: Mutex::new(BrokerInner {
                registry: LruCache::new(config.max_projects),
                last_used: HashMap::new(),
                opening_locks: HashMap::new(),
            }),
            semantic_admission: SemanticAdmission::new(
                config.semantic_permits,
                config.semantic_waiters,
                config.semantic_admission_timeout,
            ),
            config,
        });
        broker.spawn_reaper();
        broker
    }

    fn spawn_reaper(self: &Arc<Self>) {
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        handle.spawn(async move {
            let mut tick = tokio::time::interval(REAPER_TICK);
            // First tick fires immediately; skip it so the test contract
            // (`reap_idle` is the only thing that can evict synchronously)
            // is undisturbed.
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(broker) = weak.upgrade() else {
                    break;
                };
                broker.reap_idle();
            }
        });
    }

    /// Apply the resolution chain. Public so callers can pre-flight the
    /// resolution without opening a project.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the explicit / env /
    /// config path is unusable or no lakefile is reachable from `cwd`.
    pub fn resolve(&self, hint: &ProjectHint) -> Result<PathBuf> {
        if let ProjectHint::Explicit(p) = hint {
            return canonicalise(p);
        }
        if let Some(env) = &self.config.env_default {
            return canonicalise(env);
        }
        if let Some(found) = walk_up_for_lakefile(&self.config.cwd) {
            return canonicalise(&found);
        }
        if let Some(cfg) = &self.config.config_default {
            return canonicalise(cfg);
        }
        Err(ServerError::BadProject(format!(
            "no lakefile found from {} and no env/config default set",
            self.config.cwd.display()
        )))
    }

    /// Resolve the hint and load the project's [`LakeProjectMeta`] without
    /// opening (or touching) a worker. Tools that only need filesystem-level
    /// information about the project so a broken worker bootstrap can't block
    /// a pure filesystem operation.
    ///
    /// # Errors
    ///
    /// As [`Self::resolve`], plus [`ServerError::BadProject`] when the
    /// lakefile cannot be parsed or the manifest cannot be fingerprinted.
    pub fn resolve_meta(&self, hint: &ProjectHint) -> Result<LakeProjectMeta> {
        let root = self.resolve(hint)?;
        LakeProjectMeta::from_explicit(&root)
    }

    /// Process one module query through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn process_module_query(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
    ) -> Result<BrokerCall<LeanWorkerModuleQueryOutcome>> {
        let project = self.project_for(hint).await?;
        let call = project
            .process_module_query(session_imports, source, query, options)
            .await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Process one cacheable module query through the project runtime.
    pub(crate) async fn process_cached_module_query(
        &self,
        hint: ProjectHint,
        path: PathBuf,
        content_hash: [u8; 32],
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        source: String,
        query: LeanWorkerModuleQuery,
        options: LeanWorkerElabOptions,
    ) -> Result<CachedBrokerCall<LeanWorkerModuleQueryOutcome>> {
        let project = self.project_for(hint).await?;
        let key = ModuleQueryKey::from_query(&query);
        if let Some(value) = project.module_query_cache().get(&path, content_hash, &key) {
            return Ok(CachedBrokerCall {
                value,
                runtime: project.runtime_facts(),
                freshness: project.freshness(&freshness_imports),
                freshly_processed: false,
            });
        }
        let call = project
            .process_module_query(session_imports, source, query, options)
            .await?;
        project
            .module_query_cache()
            .insert(path, content_hash, key, call.value.clone());
        Ok(CachedBrokerCall {
            value: call.value,
            runtime: call.runtime,
            freshness: project.freshness(&freshness_imports),
            freshly_processed: true,
        })
    }

    /// Process one module-query batch through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn process_module_query_batch(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
    ) -> Result<BrokerCall<LeanWorkerModuleQueryBatchOutcome>> {
        let project = self.project_for(hint).await?;
        let call = project
            .process_module_query_batch(session_imports, source, selectors, budgets, options)
            .await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Process one cacheable module-query batch through the project runtime.
    pub(crate) async fn process_cached_module_query_batch(
        &self,
        hint: ProjectHint,
        path: PathBuf,
        content_hash: [u8; 32],
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        source: String,
        selectors: Vec<LeanWorkerModuleQuerySelector>,
        budgets: LeanWorkerOutputBudgets,
        options: LeanWorkerElabOptions,
    ) -> Result<CachedBrokerCall<LeanWorkerModuleQueryBatchOutcome>> {
        let project = self.project_for(hint).await?;
        let key = ModuleQueryBatchKey::from_batch(&selectors, &budgets);
        if let Some(value) = project.module_query_cache().get_batch(&path, content_hash, &key) {
            return Ok(CachedBrokerCall {
                value,
                runtime: project.runtime_facts(),
                freshness: project.freshness(&freshness_imports),
                freshly_processed: false,
            });
        }
        let call = project
            .process_module_query_batch(session_imports, source, selectors, budgets, options)
            .await?;
        project
            .module_query_cache()
            .insert_batch(path, content_hash, key, call.value.clone());
        Ok(CachedBrokerCall {
            value: call.value,
            runtime: call.runtime,
            freshness: project.freshness(&freshness_imports),
            freshly_processed: true,
        })
    }

    /// Inspect one declaration through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn inspect_declaration(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: LeanWorkerDeclarationInspectionRequest,
    ) -> Result<BrokerCall<LeanWorkerDeclarationInspectionResult>> {
        let project = self.project_for(hint).await?;
        let call = project.inspect_declaration(session_imports, request).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Run declaration search through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn search_declarations(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: LeanWorkerDeclarationSearch,
    ) -> Result<BrokerCall<LeanWorkerDeclarationSearchResult>> {
        let project = self.project_for(hint).await?;
        let call = project.search_declarations(session_imports, request).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Try proof fragments through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn attempt_proof(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: LeanWorkerProofAttemptRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<BrokerCall<LeanWorkerProofAttemptResult>> {
        let project = self.project_for(hint).await?;
        let call = project.attempt_proof(session_imports, request, options).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Verify one declaration through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, admission, or worker runtime failures.
    pub async fn verify_declaration(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<BrokerCall<LeanWorkerDeclarationVerificationResult>> {
        let project = self.project_for(hint).await?;
        let call = project.verify_declaration(session_imports, request, options).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Return runtime/freshness metadata for a project without submitting worker work.
    ///
    /// # Errors
    ///
    /// Returns resolution or project-open failures.
    pub async fn project_runtime(&self, hint: ProjectHint, freshness_imports: Vec<String>) -> Result<BrokerCall<()>> {
        let project = self.project_for(hint).await?;
        let out = BrokerCall {
            value: (),
            runtime: project.runtime_facts(),
            freshness: project.freshness(&freshness_imports),
        };
        drop(project);
        Ok(out)
    }

    async fn project_for(&self, hint: ProjectHint) -> Result<Arc<LeanProject>> {
        let root = self.resolve(&hint)?;
        self.acquire(root).await
    }

    /// Look up or open the project for `root`. Mutex is released around
    /// the slow path so concurrent calls for different roots parallelize.
    async fn acquire(&self, root: PathBuf) -> Result<Arc<LeanProject>> {
        // Fast path: registry hit + matching manifest hash.
        let cached = {
            let mut inner = self.inner.lock();
            inner.registry.get(&root).cloned()
        };
        if let Some(project) = cached {
            let current_hash = fingerprint_lake_project(&root)?;
            if project.manifest_hash() == current_hash && project.is_healthy() {
                self.inner.lock().last_used.insert(root, Instant::now());
                return Ok(project);
            }
            // Stale entry (manifest changed or actor died); evict and fall
            // through to reopen. Without the `is_healthy` check the next
            // caller would receive a `SessionGone` for every tool call until
            // the LRU happened to evict the corpse.
            let stale = {
                let mut inner = self.inner.lock();
                inner.last_used.remove(&root);
                inner.registry.pop(&root)
            };
            if let Some(s) = stale {
                s.shutdown();
            }
            drop(project);
        }

        let open_lock = {
            let mut inner = self.inner.lock();
            Arc::clone(
                inner
                    .opening_locks
                    .entry(root.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        let _open_guard = open_lock.lock().await;

        let cached_after_wait = {
            let mut inner = self.inner.lock();
            inner.registry.get(&root).cloned()
        };
        if let Some(project) = cached_after_wait {
            let current_hash = fingerprint_lake_project(&root)?;
            if project.manifest_hash() == current_hash && project.is_healthy() {
                self.inner.lock().opening_locks.remove(&root);
                self.inner.lock().last_used.insert(root, Instant::now());
                return Ok(project);
            }
            let stale = {
                let mut inner = self.inner.lock();
                inner.last_used.remove(&root);
                inner.registry.pop(&root)
            };
            if let Some(s) = stale {
                s.shutdown();
            }
        }

        // Slow path: open without holding the registry lock.
        let meta_root = root.clone();
        let meta = match tokio::task::spawn_blocking(move || LakeProjectMeta::from_explicit(&meta_root)).await {
            Ok(Ok(meta)) => meta,
            Ok(Err(err)) => {
                self.inner.lock().opening_locks.remove(&root);
                return Err(err);
            }
            Err(err) => {
                self.inner.lock().opening_locks.remove(&root);
                return Err(ServerError::Internal(format!("project metadata task failed: {err}")));
            }
        };
        let admission = Arc::clone(&self.semantic_admission);
        let opened = match tokio::task::spawn_blocking(move || LeanProject::open_with_admission(meta, admission)).await
        {
            Ok(Ok(project)) => project,
            Ok(Err(err)) => {
                self.inner.lock().opening_locks.remove(&root);
                return Err(err);
            }
            Err(err) => {
                self.inner.lock().opening_locks.remove(&root);
                return Err(ServerError::Internal(format!("project open task failed: {err}")));
            }
        };

        // Reacquire, race-resolve, insert with possible eviction.
        let mut inner = self.inner.lock();
        let (project, victim) = if let Some(existing) = inner.registry.get(&root).cloned() {
            // Someone else won the race. Use theirs; our `opened` will
            // shut down when the local `Arc` drops.
            inner.last_used.insert(root.clone(), Instant::now());
            (existing, Some(opened))
        } else {
            let victim = if inner.registry.len() >= inner.registry.cap().get() {
                pop_idle_lru(&mut inner.registry)
            } else {
                None
            };
            if victim.is_none() && inner.registry.len() >= inner.registry.cap().get() {
                inner.opening_locks.remove(&root);
                drop(inner);
                opened.shutdown();
                return Err(ServerError::WorkerUnavailable(crate::error::WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    project_root: root.to_string_lossy().into_owned(),
                    session_id: String::new(),
                    worker_generation: 0,
                    reason: "project_pool_busy_all_entries_active".to_owned(),
                }));
            }
            if let Some((ref evicted_path, _)) = victim {
                inner.last_used.remove(evicted_path);
            }
            inner.registry.put(root.clone(), Arc::clone(&opened));
            inner.last_used.insert(root.clone(), Instant::now());
            (opened, victim.map(|(_, v)| v))
        };
        inner.opening_locks.remove(&root);
        drop(inner);
        if let Some(v) = victim {
            v.shutdown();
        }
        Ok(project)
    }

    /// Evict every entry whose `last_used` timestamp is older than
    /// `config.idle_timeout`. Called every [`REAPER_TICK`] by the
    /// background task; tests call this directly to avoid waiting.
    pub fn reap_idle(&self) {
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        let evicted: Vec<Arc<LeanProject>> = {
            let mut inner = self.inner.lock();
            let expired: Vec<PathBuf> = inner
                .last_used
                .iter()
                .filter(|(_, last)| now.saturating_duration_since(**last) >= self.config.idle_timeout)
                .map(|(p, _)| p.clone())
                .collect();
            let mut out: Vec<Arc<LeanProject>> = Vec::with_capacity(expired.len());
            for p in &expired {
                if let Some(proj) = inner.registry.peek(p)
                    && !proj.is_idle()
                {
                    continue;
                }
                if let Some(proj) = inner.registry.pop(p) {
                    out.push(proj);
                }
                inner.last_used.remove(p);
            }
            out
        };
        for v in evicted {
            v.shutdown();
        }
    }

    /// Snapshot of paths currently resident in the pool, ordered MRU-first.
    /// Public so integration tests can assert eviction without going
    /// through a tool call.
    #[must_use]
    pub fn resident_paths(&self) -> Vec<PathBuf> {
        let inner = self.inner.lock();
        inner.registry.iter().map(|(p, _)| p.clone()).collect()
    }
}

fn canonicalise(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|e| ServerError::BadProject(format!("canonicalise {}: {e}", path.display())))
}

/// Walk upward from `start` looking for `lakefile.toml` / `lakefile.lean`.
/// Returns the directory containing the lakefile.
fn walk_up_for_lakefile(start: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        if dir.join("lakefile.toml").is_file() || dir.join("lakefile.lean").is_file() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

fn pop_idle_lru(registry: &mut LruCache<PathBuf, Arc<LeanProject>>) -> Option<(PathBuf, Arc<LeanProject>)> {
    let key = registry
        .iter()
        .rev()
        .find_map(|(path, project)| project.is_idle().then(|| path.clone()))?;
    let project = registry.pop(&key)?;
    Some((key, project))
}

fn broker_call<T>(project: &LeanProject, freshness_imports: &[String], call: ProjectCall<T>) -> BrokerCall<T> {
    BrokerCall {
        value: call.value,
        runtime: call.runtime,
        freshness: project.freshness(freshness_imports),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test code uses unwrap/panic to surface failure paths concisely"
)]
mod tests {
    use std::fs;

    use super::*;

    fn make_lake_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("lakefile.lean"),
            format!("package {name}\nlean_lib {}\n", name.replace('-', "_")),
        )
        .unwrap();
        dir.canonicalize().unwrap()
    }

    fn cfg(cwd: PathBuf, env: Option<PathBuf>, conf: Option<PathBuf>) -> BrokerConfig {
        BrokerConfig {
            config_default: conf,
            env_default: env,
            cwd,
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: Duration::ZERO,
            semantic_permits: NonZeroUsize::MIN,
            semantic_waiters: BrokerConfig::default_semantic_waiters(),
            semantic_admission_timeout: BrokerConfig::default_semantic_admission_timeout(),
        }
    }

    #[test]
    fn walk_up_for_lakefile_finds_in_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "myproj");
        let nested = proj.join("a/b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(walk_up_for_lakefile(&nested).unwrap(), proj);
    }

    #[test]
    fn resolve_returns_canonicalised_path_when_no_resolution_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "explicit");
        let resolved = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None))
            .resolve(&ProjectHint::Explicit(proj.clone()))
            .unwrap();
        assert_eq!(resolved, proj);
    }

    #[test]
    fn parse_pool_config_uses_defaults_when_unset() {
        let (max, idle, semantic, waiters, timeout) = parse_pool_config(None, None, None, None, None).unwrap();
        assert_eq!(max.get(), DEFAULT_MAX_PROJECTS);
        assert_eq!(idle, Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS));
        assert_eq!(semantic.get(), DEFAULT_SEMANTIC_PERMITS);
        assert_eq!(waiters.get(), DEFAULT_SEMANTIC_WAITERS);
        assert_eq!(
            timeout,
            Duration::from_millis(DEFAULT_SEMANTIC_ADMISSION_TIMEOUT_MILLIS)
        );
    }

    #[test]
    fn parse_pool_config_accepts_explicit_values() {
        let (max, idle, semantic, waiters, timeout) =
            parse_pool_config(Some("8"), Some("30"), Some("2"), Some("12"), Some("250")).unwrap();
        assert_eq!(max.get(), 8);
        assert_eq!(idle, Duration::from_secs(30));
        assert_eq!(semantic.get(), 2);
        assert_eq!(waiters.get(), 12);
        assert_eq!(timeout, Duration::from_millis(250));
    }

    #[test]
    fn parse_pool_config_treats_zero_idle_as_disable() {
        let (_, idle, _, _, _) = parse_pool_config(None, Some("0"), None, None, None).unwrap();
        assert_eq!(idle, Duration::ZERO);
    }

    #[test]
    fn parse_pool_config_rejects_max_projects_zero() {
        let err = parse_pool_config(Some("0"), None, None, None, None).unwrap_err();
        assert!(matches!(err, ServerError::Internal(_)), "{err:?}");
    }

    #[test]
    fn parse_pool_config_rejects_garbage() {
        assert!(parse_pool_config(Some("seven"), None, None, None, None).is_err());
        assert!(parse_pool_config(None, Some("forever"), None, None, None).is_err());
        assert!(parse_pool_config(None, None, Some("many"), None, None).is_err());
        assert!(parse_pool_config(None, None, Some("0"), None, None).is_err());
        assert!(parse_pool_config(None, None, None, Some("0"), None).is_err());
        assert!(parse_pool_config(None, None, None, None, Some("0")).is_err());
    }
}
