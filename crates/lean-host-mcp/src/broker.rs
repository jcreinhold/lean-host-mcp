//! `ProjectBroker`: the mediator between MCP tool dispatch and
//! the private per-project Lean runtime.
//!
//! Two responsibilities:
//!
//! 1. **Resolve** a tool call's [`ProjectHint`] into a canonical Lake-root
//!    path via the five-step chain
//!    *explicit → env → cwd-walk → config-default → error*.
//! 2. **Dispatch** typed semantic operations to a private per-project
//!    controller, opening the project lazily on first use, reusing it on
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
//! **Manifest invalidation.** Every cache hit compares the project's
//! `lake-manifest.json` hash and treats a mismatch as a miss: the project is
//! shut down and re-spawned. The hash comes from [`ProjectBroker::meta`], which
//! rebuilds a project's [`LakeProjectMeta`] only when the files it derives from
//! have changed, so the steady-state cost is five `stat` calls per tool call
//! rather than a lakefile parse and a manifest hash.
//!
//! **Slow-path concurrency.** The registry mutex is never held across
//! worker spawn or command execution. Concurrent misses for the same
//! canonical root are coalesced so only one project controller is opened.
//!
//! **Admission.** There is no broker-level gate. The resource that needs
//! protecting is one worker child's serialized request stream, and that is
//! already structural: each project owns a dedicated OS thread that runs one
//! job at a time, fed by a bounded mailbox that sheds retryably when full. A
//! process-wide semaphore on top of that would protect nothing the actor does
//! not, while serializing calls to *different* projects that share no worker.
//! Concurrency is therefore [`BrokerConfig::max_projects`] workers × one
//! in-flight job each.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lean_rs_worker_parent::{
    LeanWorkerDeclarationInspectionRequest, LeanWorkerDeclarationInspectionResult, LeanWorkerDeclarationSearch,
    LeanWorkerDeclarationSearchResult, LeanWorkerDeclarationVerificationBatchRequest,
    LeanWorkerDeclarationVerificationBatchResult, LeanWorkerDeclarationVerificationRequest,
    LeanWorkerDeclarationVerificationResult, LeanWorkerElabOptions, LeanWorkerModuleQuery,
    LeanWorkerModuleQueryBatchOutcome, LeanWorkerModuleQueryOutcome, LeanWorkerModuleQuerySelector,
    LeanWorkerOutputBudgets, LeanWorkerProofAttemptRequest, LeanWorkerProofAttemptResult,
};
use lru::LruCache;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::cache::{ModuleQueryBatchKey, ModuleQueryKey};
use crate::config_file::BrokerFileConfig;
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError};
use crate::lake_meta::{LakeProjectMeta, ProjectInputStamp};
use crate::project::{LeanProject, ProjectCall, ProjectRuntimeConfig};
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};

/// Default pool capacity when `LEAN_HOST_MCP_MAX_PROJECTS` is unset.
pub const DEFAULT_MAX_PROJECTS: usize = 4;

/// Default idle-eviction window (10 min) when
/// `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` is unset.
///
/// Long enough that an interactive Claude session keeps its projects warm,
/// short enough that a forgotten project tab releases its worker child
/// within a reasonable bound.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

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
    pub fn pool_from_env() -> Result<(NonZeroUsize, Duration)> {
        Self::pool_from_env_with_file(&BrokerFileConfig::default())
    }

    /// Resolve the pool knobs with a config-file section beneath the env vars:
    /// each knob is `env var > file > built-in default`. The same zero/deadlock
    /// guards apply to a file-sourced value as to an env-sourced one.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var is set but unparseable, or a
    /// resolved value (env or file) is zero where zero would deadlock.
    pub fn pool_from_env_with_file(file: &BrokerFileConfig) -> Result<(NonZeroUsize, Duration)> {
        parse_pool_config(
            std::env::var("LEAN_HOST_MCP_MAX_PROJECTS").ok().as_deref(),
            std::env::var("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS").ok().as_deref(),
            file,
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
}

/// Pure parser shared by [`BrokerConfig::pool_from_env_with_file`] and unit
/// tests. Each knob resolves `env > file > default`; zero/deadlock guards apply
/// to the resolved value whatever its source.
fn parse_pool_config(
    max: Option<&str>,
    idle: Option<&str>,
    file: &BrokerFileConfig,
) -> Result<(NonZeroUsize, Duration)> {
    let max_projects = nonzero(
        resolve_usize(
            "LEAN_HOST_MCP_MAX_PROJECTS",
            max,
            file.max_projects,
            DEFAULT_MAX_PROJECTS,
        )?,
        "LEAN_HOST_MCP_MAX_PROJECTS=0 would deadlock the pool",
    )?;
    // Idle timeout intentionally allows 0 (disables idle eviction).
    let idle_timeout = Duration::from_secs(resolve_u64(
        "LEAN_HOST_MCP_IDLE_TIMEOUT_SECS",
        idle,
        file.idle_timeout_secs,
        DEFAULT_IDLE_TIMEOUT_SECS,
    )?);
    Ok((max_projects, idle_timeout))
}

/// Resolve a `usize` knob through `env > file > default`. The env string is
/// parsed here; the file value is already typed.
fn resolve_usize(name: &str, env: Option<&str>, file: Option<usize>, default: usize) -> Result<usize> {
    match env {
        Some(s) => s
            .parse()
            .map_err(|e| ServerError::Internal(format!("{name}={s:?} not a usize: {e}"))),
        None => Ok(file.unwrap_or(default)),
    }
}

/// Resolve a `u64` knob through `env > file > default`.
fn resolve_u64(name: &str, env: Option<&str>, file: Option<u64>, default: u64) -> Result<u64> {
    match env {
        Some(s) => s
            .parse()
            .map_err(|e| ServerError::Internal(format!("{name}={s:?} not a u64: {e}"))),
        None => Ok(file.unwrap_or(default)),
    }
}

fn nonzero(value: usize, zero_message: &str) -> Result<NonZeroUsize> {
    NonZeroUsize::new(value).ok_or_else(|| ServerError::Internal(zero_message.to_owned()))
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
    /// Last-built [`LakeProjectMeta`] per canonical root, with the stamp of the
    /// files it came from. Purely a cost cache: [`ProjectBroker::meta`] revalidates
    /// against disk on every lookup, so an entry is never served stale. Sized to
    /// the project pool — a root the pool cannot hold is a root whose metadata we
    /// need only for the cheap non-worker paths, where a rebuild is affordable.
    meta_cache: LruCache<PathBuf, (ProjectInputStamp, LakeProjectMeta)>,
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
    runtime_config: ProjectRuntimeConfig,
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
        Self::new_with_runtime_config(config, ProjectRuntimeConfig::default())
    }

    /// Construct a broker with an explicit private project-runtime policy.
    ///
    /// The binary uses this to pass startup-parsed runtime env once, rather
    /// than having project open reread process environment on every miss.
    #[must_use]
    pub fn new_with_runtime_config(config: BrokerConfig, runtime_config: ProjectRuntimeConfig) -> Arc<Self> {
        let broker = Arc::new(Self {
            inner: Mutex::new(BrokerInner {
                registry: LruCache::new(config.max_projects),
                last_used: HashMap::new(),
                opening_locks: HashMap::new(),
                meta_cache: LruCache::new(config.max_projects),
            }),
            runtime_config,
            config,
        });
        broker.spawn_reaper();
        broker
    }

    /// The configured per-request worker deadline, in milliseconds. Tools that
    /// fan a single call out across many worker requests (project-scope
    /// `find_references`) reuse it as an overall wall-clock budget so the whole
    /// call stays bounded, not just each individual request.
    #[must_use]
    pub fn request_timeout_millis(&self) -> u64 {
        self.runtime_config.request_timeout_millis()
    }

    /// Return the cheap broker-status fields that do not require opening a
    /// project or touching a worker.
    #[must_use]
    pub fn config_snapshot(&self) -> BrokerConfigSnapshot {
        BrokerConfigSnapshot {
            max_projects: self.config.max_projects.get(),
            idle_timeout_secs: self.config.idle_timeout.as_secs(),
            request_timeout_millis: self.runtime_config.request_timeout_millis(),
            lean_max_memory_kib: self.runtime_config.lean_max_memory_kib(),
        }
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
        self.meta(&root)
    }

    /// The project metadata for an already-canonical `root`, rebuilt only when
    /// the files it derives from have changed.
    ///
    /// Every tool call needs this — the manifest hash decides whether a resident
    /// project is still current, and the canonical root and toolchain go into
    /// every response envelope. Building it costs ~55 µs of filesystem work,
    /// which on a warm module-query cache hit would be most of the call. The
    /// cache is transparent: [`LakeProjectMeta::input_stamp`] revalidates
    /// against disk on every lookup, so a caller can never observe a value that
    /// differs from a fresh build.
    ///
    /// Returned by value rather than as an `Arc`: the clone is a handful of
    /// short strings, three orders of magnitude below the build it replaces,
    /// and it keeps the cache invisible to every caller.
    ///
    /// # Errors
    ///
    /// [`ServerError::BadProject`] when the lakefile cannot be parsed or the
    /// manifest cannot be fingerprinted.
    fn meta(&self, root: &Path) -> Result<LakeProjectMeta> {
        // Revalidate outside the lock: the stamp is five `stat` calls, and the
        // registry mutex also guards the project pool.
        let cached = self.inner.lock().meta_cache.get(root).cloned();
        if let Some((stamp, meta)) = cached
            && meta.input_stamp() == stamp
        {
            return Ok(meta);
        }
        let meta = LakeProjectMeta::from_explicit(root)?;
        let stamp = meta.input_stamp();
        self.inner
            .lock()
            .meta_cache
            .put(root.to_path_buf(), (stamp, meta.clone()));
        Ok(meta)
    }

    /// Process one module query through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
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
        // A query this build cannot key is run uncached rather than filed under
        // a shared key; see `ModuleQueryKey`.
        let key = ModuleQueryKey::from_query(&query);
        // The lookup is a plain in-process map probe on the project handle: a
        // hit answers without ever entering the actor's mailbox, so a warm
        // repeat of an unmodified file never queues behind in-flight Lean work.
        if let Some(value) = key
            .as_ref()
            .and_then(|key| project.module_query_cache().get(&path, content_hash, key))
        {
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
        let (value, runtime) = call.into_parts();
        if let Some(key) = key {
            project
                .module_query_cache()
                .insert(path, content_hash, key, value.clone());
        }
        Ok(CachedBrokerCall {
            value,
            runtime,
            freshness: project.freshness(&freshness_imports),
            freshly_processed: true,
        })
    }

    /// Process one module-query batch through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
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
        // As in `process_cached_module_query`: a hit bypasses the mailbox, and
        // an unkeyable batch runs uncached.
        if let Some(value) = key
            .as_ref()
            .and_then(|key| project.module_query_cache().get_batch(&path, content_hash, key))
        {
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
        let (value, runtime) = call.into_parts();
        if let Some(key) = key {
            project
                .module_query_cache()
                .insert_batch(path, content_hash, key, value.clone());
        }
        Ok(CachedBrokerCall {
            value,
            runtime,
            freshness: project.freshness(&freshness_imports),
            freshly_processed: true,
        })
    }

    /// Inspect one declaration through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
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

    /// Run a group of declaration searches through the project runtime, against
    /// one worker session. Results come back in request order.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
    pub async fn search_declarations(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        requests: Vec<LeanWorkerDeclarationSearch>,
    ) -> Result<BrokerCall<Vec<LeanWorkerDeclarationSearchResult>>> {
        let project = self.project_for(hint).await?;
        let call = project.search_declarations(session_imports, requests).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Run source-backed semantic proof search through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, semantic capability, or
    /// worker runtime failures.
    pub(crate) async fn semantic_proof_search(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: SemanticProofSearchRequest,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let project = self.project_for(hint).await?;
        let call = project.semantic_proof_search(session_imports, request).await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Try proof fragments through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
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
    /// Returns resolution, project-open, or worker runtime failures.
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

    /// Verify several declarations in one source snapshot through the project runtime.
    ///
    /// # Errors
    ///
    /// Returns resolution, project-open, or worker runtime failures.
    pub async fn verify_declaration_batch(
        &self,
        hint: ProjectHint,
        session_imports: Vec<String>,
        freshness_imports: Vec<String>,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
    ) -> Result<BrokerCall<LeanWorkerDeclarationVerificationBatchResult>> {
        let project = self.project_for(hint).await?;
        let call = project
            .verify_declaration_batch(session_imports, request, options)
            .await?;
        let out = broker_call(&project, &freshness_imports, call);
        drop(project);
        Ok(out)
    }

    /// Return project identity without opening a worker.
    ///
    /// # Errors
    ///
    /// Returns resolution or Lake metadata failures.
    pub fn project_identity_without_worker(
        &self,
        hint: &ProjectHint,
        freshness_imports: Vec<String>,
    ) -> Result<BrokerCall<()>> {
        let meta = self.resolve_meta(hint)?;
        Ok(BrokerCall {
            value: (),
            runtime: RuntimeFacts::default(),
            freshness: freshness_from_meta(&meta, freshness_imports),
        })
    }

    /// Return runtime/freshness metadata for a project, opening its worker if
    /// this is the first call.
    ///
    /// # Errors
    ///
    /// Returns resolution or project-open failures.
    pub async fn admitted_project_runtime(
        &self,
        hint: ProjectHint,
        freshness_imports: Vec<String>,
    ) -> Result<BrokerCall<()>> {
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
            let current_hash = self.meta(&root)?.manifest_hash;
            if project.manifest_hash() == current_hash && project.is_healthy() {
                tracing::debug!(project = %root.display(), cache_hit = true, "reusing resident project");
                self.inner.lock().last_used.insert(root, Instant::now());
                return Ok(project);
            }
            tracing::debug!(
                project = %root.display(),
                manifest_changed = project.manifest_hash() != current_hash,
                healthy = project.is_healthy(),
                "evicting stale project before reopen"
            );
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
            let current_hash = self.meta(&root)?.manifest_hash;
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
        let runtime_config = self.runtime_config.clone();
        let opened = match tokio::task::spawn_blocking(move || LeanProject::open(meta, runtime_config)).await {
            Ok(Ok(project)) => project,
            Ok(Err(err)) => {
                self.inner.lock().opening_locks.remove(&root);
                tracing::warn!(project = %root.display(), error = %err, "project open failed");
                return Err(err);
            }
            Err(err) => {
                self.inner.lock().opening_locks.remove(&root);
                return Err(ServerError::Internal(format!("project open task failed: {err}")));
            }
        };
        tracing::info!(project = %root.display(), toolchain = %opened.toolchain(), "opened project; worker spawned");

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
                tracing::warn!(
                    project = %root.display(),
                    "project pool full and every entry is active; rejecting (retryable)"
                );
                return Err(ServerError::worker_unavailable(crate::error::WorkerUnavailable {
                    retryable: true,
                    worker_restarted: false,
                    project_root: root.to_string_lossy().into_owned(),
                    project_hash: String::new(),
                    imports: Vec::new(),
                    session_id: String::new(),
                    lean_toolchain: String::new(),
                    worker_generation: 0,
                    reason: "project_pool_busy_all_entries_active".to_owned(),
                    restart_cause: None,
                    rss_kib: None,
                    limit_kib: None,
                    retry_after_millis: None,
                    restarts_in_window: None,
                    window_millis: None,
                    runtime: crate::envelope::RuntimeFacts::default(),
                    // Pool exhaustion is not toolchain-provenance related, and the
                    // rejected project never opened, so there are no advisories.
                    toolchain_advisories: Vec::new(),
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
            tracing::debug!(evicted = %v.canonical_root().display(), "evicted LRU project to make room");
            v.shutdown();
        }
        Ok(project)
    }

    /// Evict every entry whose `last_used` timestamp is older than
    /// `config.idle_timeout`. Called every `REAPER_TICK` by the
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
        if !evicted.is_empty() {
            tracing::info!(
                evicted_count = evicted.len(),
                idle_timeout_secs = self.config.idle_timeout.as_secs(),
                "idle reaper evicted projects"
            );
        }
        for v in evicted {
            v.shutdown();
        }
    }

    /// Stop all resident projects and clear broker-local project state.
    ///
    /// This is the host-level shutdown policy above `lean-rs-worker-parent`:
    /// stop accepting new project work, terminalize any queued project messages,
    /// and let each project controller drive its worker child through the upstream
    /// bounded shutdown path. It is idempotent so both explicit server exits
    /// and `Drop` can call it.
    pub fn shutdown_all(&self) {
        let projects: Vec<Arc<LeanProject>> = {
            let mut inner = self.inner.lock();
            let paths: Vec<PathBuf> = inner.registry.iter().map(|(path, _)| path.clone()).collect();
            let mut projects = Vec::with_capacity(paths.len());
            for path in paths {
                if let Some(project) = inner.registry.pop(&path) {
                    projects.push(project);
                }
                inner.last_used.remove(&path);
            }
            inner.opening_locks.clear();
            projects
        };
        if !projects.is_empty() {
            tracing::info!(project_count = projects.len(), "shutting down resident projects");
        }
        for project in projects {
            project.shutdown();
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

impl Drop for ProjectBroker {
    fn drop(&mut self) {
        self.shutdown_all();
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
    let (value, runtime) = call.into_parts();
    BrokerCall {
        value,
        runtime,
        freshness: project.freshness(freshness_imports),
    }
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct BrokerConfigSnapshot {
    pub max_projects: usize,
    pub idle_timeout_secs: u64,
    pub request_timeout_millis: u64,
    /// The Lean heap ceiling each worker child runs under. Reported because it
    /// is the one memory bound a caller can hit: crossing it surfaces as an
    /// ordinary elaboration failure, and knowing the number is what turns that
    /// failure into "raise the budget" rather than "the tool is broken".
    pub lean_max_memory_kib: u64,
}

fn freshness_from_meta(meta: &LakeProjectMeta, imports: Vec<String>) -> Freshness {
    Freshness {
        project_root: meta.canonical_root.to_string_lossy().into_owned(),
        project_hash: meta.manifest_hash.clone(),
        imports,
        session_id: "metadata-only".to_owned(),
        lean_toolchain: meta.toolchain.clone(),
        toolchain_advisories: Vec::new(),
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
        fs::write(dir.join("lean-toolchain"), "leanprover/lean4:v4.34.0-rc2\n").unwrap();
        fs::write(dir.join("lake-manifest.json"), "{}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn cfg(cwd: PathBuf, env: Option<PathBuf>, conf: Option<PathBuf>) -> BrokerConfig {
        BrokerConfig {
            config_default: conf,
            env_default: env,
            cwd,
            max_projects: BrokerConfig::default_max_projects(),
            idle_timeout: Duration::ZERO,
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
        let empty = BrokerFileConfig::default();
        let (max, idle) = parse_pool_config(None, None, &empty).unwrap();
        assert_eq!(max.get(), DEFAULT_MAX_PROJECTS);
        assert_eq!(idle, Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS));
    }

    #[test]
    fn parse_pool_config_accepts_explicit_values() {
        let empty = BrokerFileConfig::default();
        let (max, idle) = parse_pool_config(Some("8"), Some("30"), &empty).unwrap();
        assert_eq!(max.get(), 8);
        assert_eq!(idle, Duration::from_secs(30));
    }

    #[test]
    fn shutdown_all_without_resident_projects_is_idempotent() {
        let broker = ProjectBroker::new(cfg(PathBuf::from("/"), None, None));
        broker.shutdown_all();
        broker.shutdown_all();
        assert!(broker.resident_paths().is_empty());
        drop(broker);
    }

    #[test]
    fn identity_without_worker_does_not_open_resident_project() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "identity");
        let broker = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None));

        let call = broker
            .project_identity_without_worker(&ProjectHint::Explicit(proj.clone()), vec!["Init".to_owned()])
            .unwrap();

        assert_eq!(call.freshness.project_root, proj.to_string_lossy());
        assert_eq!(call.freshness.session_id, "metadata-only");
        assert_eq!(call.freshness.imports, vec!["Init"]);
        assert_eq!(call.runtime.worker_generation, 0);
        assert!(!call.runtime.worker_restarted);
        assert_eq!(call.runtime.restarts_total, 0);
        assert!(
            broker.resident_paths().is_empty(),
            "identity path must not open a LeanProject"
        );
        drop(broker);
    }

    /// The metadata memo is a cost cache, so the contract is that no caller can
    /// tell it is there. Edit an input and the very next lookup must reflect the
    /// edit — the manifest hash gates project respawn, so a stale one silently
    /// pins a server to a dependency set the project has moved off of.
    #[test]
    fn resolve_meta_reflects_a_manifest_edit_on_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "memo");
        let broker = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None));
        let hint = ProjectHint::Explicit(proj.clone());

        let first = broker.resolve_meta(&hint).unwrap();
        fs::write(proj.join("lake-manifest.json"), "{\"packages\": [1]}\n").unwrap();
        let second = broker.resolve_meta(&hint).unwrap();

        assert_ne!(first.manifest_hash, second.manifest_hash);
        assert_eq!(
            second.manifest_hash,
            LakeProjectMeta::from_explicit(&proj).unwrap().manifest_hash,
            "a served value must equal what a fresh build would produce"
        );
        drop(broker);
    }

    /// The complementary case: an untouched project keeps answering equal, which
    /// is what makes the memo worth having at all. Asserted on the whole value
    /// rather than the hash alone, so a field that started coming back different
    /// under caching would fail here rather than in a tool response.
    #[test]
    fn resolve_meta_is_stable_while_the_project_is_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "stable");
        let broker = ProjectBroker::new(cfg(tmp.path().to_path_buf(), None, None));
        let hint = ProjectHint::Explicit(proj.clone());

        let first = broker.resolve_meta(&hint).unwrap();
        let second = broker.resolve_meta(&hint).unwrap();

        assert_eq!(first, second);
        assert_eq!(second, LakeProjectMeta::from_explicit(&proj).unwrap());
        drop(broker);
    }

    #[test]
    fn parse_pool_config_treats_zero_idle_as_disable() {
        let empty = BrokerFileConfig::default();
        let (_, idle) = parse_pool_config(None, Some("0"), &empty).unwrap();
        assert_eq!(idle, Duration::ZERO);
    }

    #[test]
    fn parse_pool_config_rejects_max_projects_zero() {
        let empty = BrokerFileConfig::default();
        let err = parse_pool_config(Some("0"), None, &empty).unwrap_err();
        assert!(matches!(err, ServerError::Internal(_)), "{err:?}");
    }

    #[test]
    fn parse_pool_config_rejects_garbage() {
        let e = BrokerFileConfig::default();
        assert!(parse_pool_config(Some("seven"), None, &e).is_err());
        assert!(parse_pool_config(None, Some("forever"), &e).is_err());
    }

    #[test]
    fn parse_pool_config_file_value_used_when_env_unset_and_env_wins() {
        let file = BrokerFileConfig {
            max_projects: Some(7),
            idle_timeout_secs: Some(45),
        };
        // Env unset -> file values used.
        let (max, idle) = parse_pool_config(None, None, &file).unwrap();
        assert_eq!(max.get(), 7);
        assert_eq!(idle, Duration::from_secs(45));
        // Env present -> env wins over the file value.
        let (max, idle) = parse_pool_config(Some("3"), Some("30"), &file).unwrap();
        assert_eq!(max.get(), 3);
        assert_eq!(idle, Duration::from_secs(30));
    }

    #[test]
    fn parse_pool_config_rejects_zero_max_projects_from_file() {
        let file = BrokerFileConfig {
            max_projects: Some(0),
            ..BrokerFileConfig::default()
        };
        assert!(parse_pool_config(None, None, &file).is_err());
    }
}
