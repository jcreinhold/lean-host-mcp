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
//! shut down and re-spawned. The hash comes from `ProjectBroker::meta`, which
//! rebuilds a project's [`LakeProjectMeta`] only when the files it derives from
//! have changed, so the steady-state cost is five `stat` calls per tool call
//! rather than a lakefile parse and a manifest hash.
//!
//! **Slow-path concurrency.** The registry mutex is never held across
//! worker spawn or command execution. Concurrent misses for the same
//! canonical root serialize startup and recheck the per-project worker limit.
//!
//! **Admission.** Each worker owns a serialized thread with a bounded
//! mailbox. Capacity reservations count resident, opening, and retiring workers
//! until confirmed exit. Ordinary concurrency is [`BrokerConfig::max_projects`]
//! workers × one in-flight job each. An explicit [`ProjectBroker::runtime_control`] lease
//! can atomically pause idle-pool admission for a cooperating external caller;
//! it preserves warm caches against ordinary expiry unless the caller requests
//! retirement. Independent artifact-writer demand can still retire an idle cache.

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
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::{ModuleQueryBatchKey, ModuleQueryKey};
use crate::cancellation::CallCancellation;
use crate::config_file::BrokerFileConfig;
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError};
use crate::lake_meta::{LakeProjectMeta, ProjectInputStamp};
use crate::project::{LeanProject, ProjectCall, ProjectRuntimeConfig};
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};

mod control;
pub(crate) mod module_setup;
pub(crate) use module_setup::ChargedWriter;
mod preparation;
mod semantic_file;
pub(crate) mod verify_file;
pub(crate) use verify_file::FileProjection;
mod writer_handoff;
pub(crate) use control::MaintenanceAccess;
pub use control::{
    RuntimeAction, RuntimeControlRequest, RuntimeControlResult, RuntimeControlStatus, WorkerActivity, WorkerPoolStatus,
};

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
const ARTIFACT_HANDOFF_TICK: Duration = Duration::from_millis(100);

/// Bag of broker inputs. Built once at startup from the CLI / env /
/// config; `cwd` is injectable so tests can drive the cwd-walk step
/// without `std::env::set_current_dir`.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub config_default: Option<PathBuf>,
    pub env_default: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Global worker cap. Historical API name; config prefers `max_workers`.
    /// A cold miss can evict an idle worker, never an active one.
    pub max_projects: NonZeroUsize,
    /// Independent worker processes allowed for one canonical project root.
    pub max_workers_per_project: NonZeroUsize,
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
        let workers = std::env::var("LEAN_HOST_MCP_MAX_WORKERS").ok();
        let projects = std::env::var("LEAN_HOST_MCP_MAX_PROJECTS").ok();
        parse_worker_pool_config(
            workers.as_deref(),
            projects.as_deref(),
            std::env::var("LEAN_HOST_MCP_IDLE_TIMEOUT_SECS").ok().as_deref(),
            file,
        )
    }

    /// Resolve global and per-project worker limits; old configurations retain one worker per project.
    ///
    /// # Errors
    /// Returns an error for zero or malformed per-project limits.
    pub fn worker_limits_from_file(
        file: &BrokerFileConfig,
        max_projects: NonZeroUsize,
    ) -> Result<(NonZeroUsize, NonZeroUsize)> {
        let total = max_projects;
        let per_project = nonzero(
            resolve_usize(
                "LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT",
                std::env::var("LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT").ok().as_deref(),
                file.max_workers_per_project,
                1,
            )?,
            "max_workers_per_project must be positive",
        )?;
        Ok((total, per_project))
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
fn parse_worker_pool_config(
    workers: Option<&str>,
    projects: Option<&str>,
    idle: Option<&str>,
    file: &BrokerFileConfig,
) -> Result<(NonZeroUsize, Duration)> {
    let resolved = BrokerFileConfig {
        max_projects: file.max_workers.or(file.max_projects),
        idle_timeout_secs: file.idle_timeout_secs,
        ..BrokerFileConfig::default()
    };
    parse_pool_config(workers.or(projects), idle, &resolved)
}

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkerKey {
    root: PathBuf,
    lane: usize,
}

struct BrokerInner {
    control: control::ControlState,
    registry: LruCache<WorkerKey, Arc<LeanProject>>,
    last_used: HashMap<WorkerKey, Instant>,
    opening_locks: HashMap<PathBuf, Weak<AsyncMutex<()>>>,
    shutdown_epoch: u64,
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
    inner: Arc<Mutex<BrokerInner>>,
    config: BrokerConfig,
    runtime_config: ProjectRuntimeConfig,
    worker_slots: Arc<Semaphore>,
    capacity_admission: AsyncMutex<()>,
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
    /// ([`tokio::runtime::Handle::try_current`]). Artifact handoff remains active
    /// when `idle_timeout` is zero; only ordinary expiry is disabled.
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
        runtime_config.log_advisories();
        let broker = Arc::new(Self {
            inner: Arc::new(Mutex::new(BrokerInner {
                control: control::ControlState::default(),
                registry: LruCache::new(config.max_projects),
                last_used: HashMap::new(),
                opening_locks: HashMap::new(),
                shutdown_epoch: 0,
                meta_cache: LruCache::new(config.max_projects),
            })),
            runtime_config,
            worker_slots: Arc::new(Semaphore::new(config.max_projects.get())),
            capacity_admission: AsyncMutex::new(()),
            config,
        });
        broker.spawn_reaper();
        broker
    }

    #[cfg(test)]
    pub(crate) fn test_register_actor(&self, project: Arc<LeanProject>) {
        let root = project.canonical_root().to_path_buf();
        self.inner.lock().registry.put(WorkerKey { root, lane: 0 }, project);
    }

    #[cfg(test)]
    pub(crate) fn test_maintenance_access(&self) -> MaintenanceAccess {
        MaintenanceAccess::new(&self.inner)
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
            max_workers: self.config.max_projects.get(),
            max_workers_per_project: self.config.max_workers_per_project.get(),
            idle_timeout_secs: self.config.idle_timeout.as_secs(),
            request_timeout_millis: self.runtime_config.request_timeout_millis(),
            lean_max_memory_kib: self.runtime_config.lean_max_memory_kib(),
        }
    }

    fn spawn_reaper(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        handle.spawn(async move {
            let mut idle = tokio::time::interval(REAPER_TICK);
            let mut handoff = tokio::time::interval(ARTIFACT_HANDOFF_TICK);
            idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            handoff.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            idle.tick().await;
            handoff.tick().await;
            loop {
                let reap_idle = tokio::select! {
                    _ = idle.tick() => true,
                    _ = handoff.tick() => false,
                };
                let Some(broker) = weak.upgrade() else {
                    break;
                };
                // Filesystem/lock observation and retirement stay off the async
                // executor. Writer handoff remains enabled when idle expiry is 0.
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    if reap_idle {
                        broker.reap_idle();
                    } else {
                        broker.reap_artifact_handoffs();
                    }
                })
                .await
                {
                    tracing::warn!(%error, "broker housekeeping failed; later ticks will observe again");
                }
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .process_module_query(session_imports, source, query, options, admission_deadline)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        // A query this build cannot key is run uncached rather than filed under
        // a shared key; see `ModuleQueryKey`.
        let key = ModuleQueryKey::from_query(&query);
        let context = crate::cache::ModuleQueryContext::capture(
            project.canonical_root(),
            &session_imports,
            &freshness_imports,
            &options,
            project.runtime_facts().worker_generation,
        );
        // The lookup is a plain in-process map probe on the project handle: a
        // hit answers without ever entering the actor's mailbox, so a warm
        // repeat of an unmodified file never queues behind in-flight Lean work.
        if let Some(value) = key
            .as_ref()
            .zip(context.as_ref())
            .and_then(|(key, context)| project.module_query_cache().get(&path, content_hash, context, key))
            .filter(|_| {
                context.as_ref().is_some_and(|context| {
                    context
                        .after_processing(project.canonical_root(), project.runtime_facts().worker_generation)
                        .as_ref()
                        == Some(context)
                })
            })
        {
            return Ok(CachedBrokerCall {
                value,
                runtime: project.runtime_facts(),
                freshness: Freshness {
                    elaboration_options: Some(option_facts.clone()),
                    ..project.freshness(&freshness_imports)
                },
                freshly_processed: false,
            });
        }
        let call = project
            .process_module_query(session_imports, source, query, options, admission_deadline)
            .await?;
        let (value, runtime) = call.into_parts();
        if let Some((key, context)) =
            key.zip(context.and_then(|context| context.after_call(project.canonical_root(), &runtime)))
        {
            project
                .module_query_cache()
                .insert(path, content_hash, context, key, value.clone());
        }
        Ok(CachedBrokerCall {
            value,
            runtime,
            freshness: Freshness {
                elaboration_options: Some(option_facts.clone()),
                ..project.freshness(&freshness_imports)
            },
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .process_module_query_batch(session_imports, source, selectors, budgets, options, admission_deadline)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let key = ModuleQueryBatchKey::from_batch(&selectors, &budgets);
        let context = crate::cache::ModuleQueryContext::capture(
            project.canonical_root(),
            &session_imports,
            &freshness_imports,
            &options,
            project.runtime_facts().worker_generation,
        );
        // As in `process_cached_module_query`: a hit bypasses the mailbox, and
        // an unkeyable batch runs uncached.
        if let Some(value) = key
            .as_ref()
            .zip(context.as_ref())
            .and_then(|(key, context)| {
                project
                    .module_query_cache()
                    .get_batch(&path, content_hash, context, key)
            })
            .filter(|_| {
                context.as_ref().is_some_and(|context| {
                    context
                        .after_processing(project.canonical_root(), project.runtime_facts().worker_generation)
                        .as_ref()
                        == Some(context)
                })
            })
        {
            return Ok(CachedBrokerCall {
                value,
                runtime: project.runtime_facts(),
                freshness: Freshness {
                    elaboration_options: Some(option_facts.clone()),
                    ..project.freshness(&freshness_imports)
                },
                freshly_processed: false,
            });
        }
        let call = project
            .process_module_query_batch(session_imports, source, selectors, budgets, options, admission_deadline)
            .await?;
        let (value, runtime) = call.into_parts();
        if let Some((key, context)) =
            key.zip(context.and_then(|context| context.after_call(project.canonical_root(), &runtime)))
        {
            project
                .module_query_cache()
                .insert_batch(path, content_hash, context, key, value.clone());
        }
        Ok(CachedBrokerCall {
            value,
            runtime,
            freshness: Freshness {
                elaboration_options: Some(option_facts.clone()),
                ..project.freshness(&freshness_imports)
            },
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
        let (project, admission_deadline) = self.project_for(hint).await?;
        let call = project
            .inspect_declaration(session_imports, request, admission_deadline)
            .await?;
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
        let (project, admission_deadline) = self.project_for(hint).await?;
        let call = project
            .search_declarations(session_imports, requests, admission_deadline)
            .await?;
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
        if !self.runtime_config.managed_semantic_companion
            && (request.goal.file_label.is_some() || self.prepared_generation().is_some())
        {
            return self
                .semantic_file(
                    hint,
                    session_imports,
                    freshness_imports,
                    request,
                    self.prepared_generation(),
                )
                .await;
        }
        let (project, admission_deadline) = self.project_for(hint).await?;
        let call = project
            .semantic_proof_search(session_imports, request, admission_deadline)
            .await?;
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .attempt_proof(session_imports, request, options, admission_deadline)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .verify_declaration(session_imports, request, options, admission_deadline)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
        drop(project);
        Ok(out)
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn managed_only(&self) -> bool {
        self.runtime_config.managed_only
    }

    pub(crate) fn prepared_generation(&self) -> Option<&str> {
        self.runtime_config.prepared_generation()
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .verify_declaration_batch(session_imports, request, options, admission_deadline)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
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

    /// Describe compiled compatibility without opening, preparing or admitting a worker.
    ///
    /// # Errors
    /// Returns project metadata, unsupported toolchain or helper-identity errors.
    pub fn describe_native_compatibility(
        &self,
        hint: &ProjectHint,
    ) -> Result<crate::runtime_compatibility::PureCompatibilityResponse> {
        let meta = self.resolve_meta(hint)?;
        crate::runtime_compatibility::PureCompatibilityResponse::describe(
            self.instance_id(),
            meta.canonical_root,
            meta.toolchain,
        )
        .map_err(ServerError::BadProject)
    }

    /// Establish compatibility through normal worker admission and an actual
    /// checked empty-import session, without executing a target proof.
    ///
    /// # Errors
    /// Returns admission, worker startup, or loaded compatibility failures.
    pub async fn native_compatibility(
        &self,
        hint: ProjectHint,
    ) -> Result<crate::runtime_compatibility::CompatibilityResponse> {
        let (project, admission_deadline) = self.project_for(hint).await?;
        let call = project.native_compatibility(admission_deadline).await?;
        let ((), runtime) = call.into_parts();
        let native_compatibility = runtime
            .native_compatibility
            .ok_or_else(|| ServerError::Internal("actual native compatibility receipt is missing".into()))?;
        Ok(crate::runtime_compatibility::CompatibilityResponse {
            status: "ok".into(),
            native_compatibility,
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
        let (project, _) = self.project_for(hint).await?;
        let out = BrokerCall {
            value: (),
            runtime: project.runtime_facts(),
            freshness: project.freshness(&freshness_imports),
        };
        drop(project);
        Ok(out)
    }

    /// All source-check preparation precedes the actor call. A typed admission
    /// expiry/cancellation here proves only that this named check was not sent.
    /// Preparation or earlier checks may already have run; their independent
    /// operation evidence still determines the retained invocation's disposition.
    async fn prepare_check(
        &self,
        hint: ProjectHint,
        options: LeanWorkerElabOptions,
    ) -> Result<(Arc<LeanProject>, LeanWorkerElabOptions, Instant)> {
        let file_label = options.file_label.clone();
        let admission = async {
            let root = self.native_root(&hint)?;
            let meta = self.meta(&root)?;
            let options = self.resolve_options(&meta, options).await?;
            let (project, admission_deadline) = self.acquire(root).await?;
            Ok((project, options, admission_deadline))
        }
        .await;
        if let Err(ServerError::WorkerUnavailable(info)) = &admission
            && matches!(info.reason.as_str(), "project_admission_timeout" | "request_cancelled")
        {
            CallCancellation::current().target_check_not_started(crate::envelope::TargetCheckNotStarted {
                file_label,
                stage: "project_admission".to_owned(),
                disposition: "not_started".to_owned(),
            });
        }
        admission
    }

    async fn resolve_options(
        &self,
        meta: &LakeProjectMeta,
        options: LeanWorkerElabOptions,
    ) -> Result<LeanWorkerElabOptions> {
        let activity = self.preparation_activity(&meta.canonical_root).await?;
        // Resolve availability before evaluating arbitrary project configuration.
        // This is metadata-only and preserves ordinary missing-worker precedence.
        let _worker = crate::project::resolve_worker(meta)?;
        crate::lake_options::resolve(
            &meta.canonical_root,
            options,
            self.config.max_projects.get(),
            Some(Box::new(activity)),
        )
        .await
    }

    pub(crate) fn native_root(&self, hint: &ProjectHint) -> Result<PathBuf> {
        let root = self.resolve(hint)?;
        self.runtime_config.require_operator_primary(&root)?;
        Ok(root)
    }

    async fn project_for(&self, hint: ProjectHint) -> Result<(Arc<LeanProject>, Instant)> {
        let root = self.native_root(&hint)?;
        self.acquire(root).await
    }

    /// Look up or open the project for `root`. Mutex is released around
    /// the slow path so concurrent calls for different roots parallelize. The
    /// original deadline continues through the returned actor's queue admission.
    async fn acquire(&self, root: PathBuf) -> Result<(Arc<LeanProject>, Instant)> {
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(self.request_timeout_millis()))
            .ok_or_else(|| ServerError::Internal("admission deadline is out of range".to_owned()))?;
        let project = self.acquire_until(root, deadline).await?;
        Ok((project, deadline.into_std()))
    }

    async fn acquire_until(&self, root: PathBuf, deadline: tokio::time::Instant) -> Result<Arc<LeanProject>> {
        let cancellation = CallCancellation::current();
        loop {
            let result = self.acquire_attempt(root.clone(), deadline).await;
            if !matches!(&result, Err(ServerError::WorkerUnavailable(info)) if matches!(info.reason.as_str(), "worker_pool_draining" | "artifact_admission_busy"))
            {
                return result;
            }
            // Waiting callers own no project Arc, opening lock, or worker slot.
            // A bounded local timer also observes expiry without another API call.
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Self::pool_unavailable(&root, "request_cancelled", false)),
                () = tokio::time::sleep_until(deadline) => return Err(Self::pool_unavailable(&root, "project_admission_timeout", true)),
                () = tokio::time::sleep(Duration::from_millis(100)) => {},
            }
        }
    }

    async fn acquire_attempt(&self, root: PathBuf, deadline: tokio::time::Instant) -> Result<Arc<LeanProject>> {
        if CallCancellation::current().is_cancelled() {
            return Err(Self::pool_unavailable(&root, "request_cancelled", false));
        }
        let epoch = self.inner.lock().shutdown_epoch;
        if let Some(project) = self.select_worker(&root)? {
            return Ok(project);
        }

        // Preparation can need EX artifact locks. Do it before retaining an
        // opening Arc or slot: the owned drain must never wait on this caller.
        let meta_root = root.clone();
        let meta = tokio::task::spawn_blocking(move || LakeProjectMeta::from_explicit(&meta_root))
            .await
            .map_err(|err| ServerError::Internal(format!("project metadata task failed: {err}")))??;
        let (worker, sysroot, _) = crate::project::resolve_worker(&meta)?;
        let writer = crate::shared_writer::contract()?.executable;
        let cancellation = CallCancellation::current();
        let prepared = self
            .prepare_host_artifacts(&root, &worker, &sysroot, &writer, deadline, &cancellation)
            .await?;
        let open_guard = self.acquire_opening_guard(&root, deadline, &cancellation).await?;

        // Root-local startup serialization closes the race between callers that
        // both saw spare capacity. Selection pins the returned actor with an Arc.
        if let Some(project) = self.select_worker(&root)? {
            return Ok(project);
        }
        let (lane, growing) = {
            let inner = self.inner.lock();
            let lane = (0..self.config.max_workers_per_project.get())
                .find(|lane| {
                    !inner.registry.contains(&WorkerKey {
                        root: root.clone(),
                        lane: *lane,
                    })
                })
                .ok_or_else(|| ServerError::Internal("no vacant project worker slot".to_owned()))?;
            (lane, inner.registry.iter().any(|(key, _)| key.root == root))
        };
        let key = WorkerKey {
            root: root.clone(),
            lane,
        };
        // Extra workers are opportunistic: never wait for capacity or evict an
        // unrelated project while this project's existing actor can accept work.
        let slot = if growing {
            let Ok(_turn) = self.capacity_admission.try_lock() else {
                return self
                    .select_existing_worker(&root)?
                    .ok_or_else(|| Self::pool_unavailable(&root, "worker_pool_busy", true));
            };
            match Arc::clone(&self.worker_slots).try_acquire_owned() {
                Ok(slot) => slot,
                Err(_) => {
                    return self
                        .select_existing_worker(&root)?
                        .ok_or_else(|| Self::pool_unavailable(&root, "worker_pool_busy", true));
                }
            }
        } else {
            self.reserve_worker_slot(&root, deadline).await?
        };
        let cancellation = CallCancellation::current();
        let mut on_drop = cancellation.cancel_on_drop();
        if cancellation.is_cancelled() {
            return Err(Self::pool_unavailable(&root, "request_cancelled", false));
        }
        let runtime_config = self.runtime_config.clone();
        let inner = Arc::clone(&self.inner);
        let maintenance = MaintenanceAccess::new(&self.inner);
        let native_capacity_limit = self.config.max_projects.get();
        let root = root.clone();
        // The blocking startup keeps running if this async caller disappears.
        // The actor owns initialization evidence independently of this waiter.
        let initialization = cancellation.begin_operation();
        let opened = tokio::task::spawn_blocking(move || {
            // Hold same-root serialization through registration even when the
            // async caller disappears while blocking startup is still running.
            let _open_guard = open_guard;
            let opened = LeanProject::open(
                meta, runtime_config, slot, maintenance, prepared, native_capacity_limit, initialization,
            )?;
            let mut registry = inner.lock();
            if registry.shutdown_epoch != epoch {
                drop(registry);
                opened.shutdown();
                return Err(Self::pool_unavailable(&root, "project_shutting_down", false));
            }
            registry.registry.put(key.clone(), Arc::clone(&opened));
            registry.last_used.insert(key, Instant::now());
            drop(registry);
            tracing::info!(project = %root.display(), toolchain = %opened.toolchain(), "opened project; worker spawned");
            if cancellation.is_cancelled() {
                // Startup already completed. Keep its idle worker warm so a
                // surviving coalesced caller can reuse it without waiting for
                // retirement or receiving a false pool-full rejection.
                return Err(Self::pool_unavailable(&root, "request_cancelled", false));
            }
            Ok(opened)
        })
        .await
        .map_err(|err| ServerError::Internal(format!("project open task failed: {err}")))?;
        on_drop.disarm();
        opened
    }

    /// Root-local serialization starts only after runtime-free preparation.
    async fn acquire_opening_guard(
        &self,
        root: &Path,
        deadline: tokio::time::Instant,
        cancellation: &CallCancellation,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let open_lock = self.opening_lock(root)?;
        let guard = tokio::select! {
            guard = tokio::time::timeout_at(deadline, open_lock.lock_owned()) => {
                guard.map_err(|_| Self::pool_unavailable(root, "project_admission_timeout", true))?
            },
            () = cancellation.cancelled() => {
                return Err(Self::pool_unavailable(root, "request_cancelled", false));
            }
        };
        if cancellation.is_cancelled() {
            return Err(Self::pool_unavailable(root, "request_cancelled", false));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Self::pool_unavailable(root, "project_admission_timeout", true));
        }
        Ok(guard)
    }

    /// Prefer a free actor; grow only while the global pool has spare capacity.
    /// A busy actor remains a valid queue, not a reason to refuse a request.
    fn select_worker(&self, root: &Path) -> Result<Option<Arc<LeanProject>>> {
        {
            let mut inner = self.inner.lock();
            Self::check_runtime_admission(&mut inner, root)?;
            let _artifact_admission = Self::check_artifact_admission(&inner, root)?;
            if !inner.registry.iter().any(|(key, _)| key.root == root) {
                return Ok(None);
            }
        }
        let manifest_hash = self.meta(root)?.manifest_hash;
        let mut inner = self.inner.lock();
        Self::check_runtime_admission(&mut inner, root)?;
        let _artifact_admission = Self::check_artifact_admission(&inner, root)?;
        let stale: Vec<_> = inner
            .registry
            .iter()
            .filter(|(key, actor)| key.root == root && (!actor.is_healthy() || actor.manifest_hash() != manifest_hash))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(actor) = inner.registry.pop(&key) {
                actor.shutdown();
            }
            inner.last_used.remove(&key);
        }
        let candidates: Vec<_> = inner
            .registry
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(key, actor)| {
                (
                    key.clone(),
                    actor
                        .outstanding_jobs()
                        .saturating_add(usize::from(Arc::strong_count(actor) > 1)),
                )
            })
            .collect();
        let Some((key, load)) = candidates.iter().min_by_key(|(_, load)| *load) else {
            return Ok(None);
        };
        if *load > 0
            && candidates.len() < self.config.max_workers_per_project.get()
            && self.worker_slots.available_permits() > 0
        {
            return Ok(None);
        }
        let key = key.clone();
        inner.last_used.insert(key.clone(), Instant::now());
        Ok(inner.registry.get(&key).cloned())
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    pub(crate) async fn managed_actor(&self, deadline: tokio::time::Instant) -> Result<Arc<LeanProject>> {
        let root = self.native_root(&ProjectHint::Default)?;
        if let Some(actor) = self.select_existing_worker(&root)? {
            return Ok(actor);
        }
        // The actor's ordinary bootstrap validates its installed shims and
        // starts a guarded receiver with an empty import set. There is no
        // OpenHostSession or target body until the managed job binds it.
        tokio::time::timeout_at(deadline, self.acquire_until(root.clone(), deadline))
            .await
            .map_err(|_| Self::pool_unavailable(&root, "project_admission_timeout", true))?
    }

    fn select_existing_worker(&self, root: &Path) -> Result<Option<Arc<LeanProject>>> {
        let mut inner = self.inner.lock();
        Self::check_runtime_admission(&mut inner, root)?;
        let _artifact_admission = Self::check_artifact_admission(&inner, root)?;
        let key = inner
            .registry
            .iter()
            .filter(|(key, actor)| key.root == root && actor.is_healthy())
            .min_by_key(|(_, actor)| {
                actor
                    .outstanding_jobs()
                    .saturating_add(usize::from(Arc::strong_count(actor) > 1))
            })
            .map(|(key, _)| key.clone());
        let Some(key) = key else { return Ok(None) };
        inner.last_used.insert(key.clone(), Instant::now());
        Ok(inner.registry.get(&key).cloned())
    }

    fn opening_lock(&self, root: &Path) -> Result<Arc<AsyncMutex<()>>> {
        let mut inner = self.inner.lock();
        // Claim acquisition uses this same mutex. A cold caller must either
        // retain its opening lock before the claim, or observe the held lease.
        Self::check_runtime_admission(&mut inner, root)?;
        let _artifact_admission = Self::check_artifact_admission(&inner, root)?;
        inner.opening_locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = inner.opening_locks.get(root).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(AsyncMutex::new(()));
        inner.opening_locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
        drop(inner);
        Ok(lock)
    }

    fn check_runtime_admission(inner: &mut BrokerInner, root: &Path) -> Result<()> {
        if let Some(remaining) = inner.control.remaining(Instant::now()) {
            let reason = if inner.control.is_draining() {
                "worker_pool_draining"
            } else {
                "worker_pool_leased"
            };
            let mut error = Self::pool_unavailable(root, reason, true);
            if let ServerError::WorkerUnavailable(info) = &mut error {
                info.retry_after_millis = Some(remaining);
            }
            return Err(error);
        }
        Ok(())
    }

    async fn reserve_worker_slot(&self, root: &Path, deadline: tokio::time::Instant) -> Result<OwnedSemaphorePermit> {
        crate::worker_retirement::poll_retirements();
        let cancellation = CallCancellation::current();
        if cancellation.is_cancelled() {
            return Err(Self::pool_unavailable(root, "request_cancelled", false));
        }
        let epoch = self.inner.lock().shutdown_epoch;
        // One FIFO admission queue owns both idle eviction and slot acquisition.
        // Later cold projects cannot steal a released slot from an earlier waiter.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(Self::pool_unavailable(root, "request_cancelled", false)),
            result = tokio::time::timeout_at(deadline, async {
                let _turn = self.capacity_admission.lock().await;
                loop {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Self::pool_unavailable(root, "project_admission_timeout", true));
                    }
                    let victim = {
                        let mut inner = self.inner.lock();
                        if inner.shutdown_epoch != epoch {
                            return Err(Self::pool_unavailable(root, "project_shutting_down", false));
                        }
                        // This caller already owns an opening-lock Arc. A drain
                        // waits for it; it must be allowed to finish startup.

                        if let Ok(slot) = Arc::clone(&self.worker_slots).try_acquire_owned() {
                            return Ok(slot);
                        }
                        let victim = pop_idle_lru(&mut inner.registry);
                        if let Some((path, _)) = &victim {
                            inner.last_used.remove(path);
                        }
                        victim
                    };
                    if let Some((path, project)) = victim {
                        tracing::debug!(evicted = %path.root.display(), "retiring idle LRU project before replacement startup");
                        project.shutdown();
                        // Do not retire another worker while this one is stopping.
                        return self.wait_for_worker_slot(root, epoch, deadline).await;
                    }
                    // Only the head waiter polls. Resident activity holds Arc and
                    // job guards; neither it nor uncertain retirement frees capacity.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }) => result.unwrap_or_else(|_| Err(Self::pool_unavailable(root, "project_admission_timeout", true))),
        }
    }

    async fn wait_for_worker_slot(
        &self,
        root: &Path,
        epoch: u64,
        deadline: tokio::time::Instant,
    ) -> Result<OwnedSemaphorePermit> {
        let cancellation = CallCancellation::current();
        let slot = Arc::clone(&self.worker_slots).acquire_owned();
        let deadline = tokio::time::sleep_until(deadline);
        tokio::pin!(slot, deadline);
        let mut shutdown_check = tokio::time::interval(Duration::from_millis(25));
        shutdown_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Self::pool_unavailable(root, "request_cancelled", false)),
                _ = shutdown_check.tick() => {
                    if self.inner.lock().shutdown_epoch != epoch {
                        return Err(Self::pool_unavailable(root, "project_shutting_down", false));
                    }
                }
                () = &mut deadline => return Err(Self::pool_unavailable(root, "project_admission_timeout", true)),
                slot = &mut slot => {
                    let slot = slot.map_err(|err| ServerError::Internal(format!("worker capacity closed: {err}")))?;
                    if self.inner.lock().shutdown_epoch != epoch {
                        return Err(Self::pool_unavailable(root, "project_shutting_down", false));
                    }
                    return Ok(slot);
                }
            }
        }
    }

    fn pool_unavailable(root: &Path, reason: &str, retryable: bool) -> ServerError {
        ServerError::worker_unavailable(crate::error::WorkerUnavailable {
            retryable,
            worker_restarted: false,
            worker_stop_receipt: None,
            preparation_closed: false,
            project_root: root.to_string_lossy().into_owned(),
            project_hash: String::new(),
            imports: Vec::new(),
            session_id: String::new(),
            lean_toolchain: String::new(),
            worker_generation: 0,
            reason: reason.to_owned(),
            restart_cause: None,
            rss_kib: None,
            limit_kib: None,
            retry_after_millis: None,
            restarts_in_window: None,
            window_millis: None,
            runtime: crate::envelope::RuntimeFacts::default(),
            toolchain_advisories: Vec::new(),
        })
    }

    /// Evict every entry whose `last_used` timestamp is older than
    /// `config.idle_timeout`. Called every `REAPER_TICK` by the
    /// background task; tests call this directly to avoid waiting.
    pub fn reap_idle(&self) {
        crate::worker_retirement::poll_retirements();
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        let evicted: Vec<Arc<LeanProject>> = {
            let mut inner = self.inner.lock();
            // A maintenance caller explicitly chose to keep warm caches.
            // Ordinary idle eviction resumes when its lease ends.
            if inner.control.remaining(now).is_some() {
                return;
            }
            let expired: Vec<WorkerKey> = inner
                .last_used
                .iter()
                .filter(|(_, last)| now.saturating_duration_since(**last) >= self.config.idle_timeout)
                .map(|(p, _)| p.clone())
                .collect();
            let mut out: Vec<Arc<LeanProject>> = Vec::with_capacity(expired.len());
            for p in &expired {
                if let Some(proj) = inner.registry.peek(p)
                    && (!proj.is_idle() || Arc::strong_count(proj) != 1)
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
            inner.shutdown_epoch = inner.shutdown_epoch.wrapping_add(1);
            let paths: Vec<WorkerKey> = inner.registry.iter().map(|(path, _)| path.clone()).collect();
            let mut projects = Vec::with_capacity(paths.len());
            for path in paths {
                if let Some(project) = inner.registry.pop(&path) {
                    projects.push(project);
                }
                inner.last_used.remove(&path);
            }
            inner.opening_locks.retain(|_, lock| lock.strong_count() > 0);
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
        let mut seen = std::collections::HashSet::new();
        inner
            .registry
            .iter()
            .filter(|(key, _)| seen.insert(key.root.clone()))
            .map(|(key, _)| key.root.clone())
            .collect()
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

fn pop_idle_lru(registry: &mut LruCache<WorkerKey, Arc<LeanProject>>) -> Option<(WorkerKey, Arc<LeanProject>)> {
    let key = registry
        .iter()
        .rev()
        .find_map(|(path, project)| (project.is_idle() && Arc::strong_count(project) == 1).then(|| path.clone()))?;
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
    pub max_workers: usize,
    pub max_workers_per_project: usize,
    pub idle_timeout_secs: u64,
    pub request_timeout_millis: u64,
    /// The Lean heap ceiling each worker child runs under. Reported because it
    /// is the worker's allocator backstop: exceeding it can abort the child.
    /// This is not a process-footprint or machine-wide memory ceiling.
    pub lean_max_memory_kib: u64,
}

fn freshness_from_meta(meta: &LakeProjectMeta, imports: Vec<String>) -> Freshness {
    Freshness {
        elaboration_options: None,
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
        fs::write(dir.join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
        fs::write(dir.join("lake-manifest.json"), "{}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn cfg(cwd: PathBuf, env: Option<PathBuf>, conf: Option<PathBuf>) -> BrokerConfig {
        BrokerConfig {
            config_default: conf,
            env_default: env,
            cwd,
            max_projects: BrokerConfig::default_max_projects(),
            max_workers_per_project: std::num::NonZeroUsize::MIN,
            idle_timeout: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn operator_primary_refuses_discovered_and_explicit_project_before_options() {
        let tree = tempfile::tempdir().unwrap();
        let approved = tree.path().join("approved");
        let other = tree.path().join("other");
        fs::create_dir(&approved).unwrap();
        fs::create_dir(&other).unwrap();
        // This marker permits discovery. There is deliberately no Lean toolchain.
        fs::write(other.join("lakefile.lean"), "").unwrap();
        let runtime = ProjectRuntimeConfig::default()
            .with_operator_primary(Some(&approved))
            .unwrap();
        let broker = ProjectBroker::new_with_runtime_config(cfg(other.clone(), None, None), runtime);
        for hint in [ProjectHint::Default, ProjectHint::Explicit(other.clone())] {
            let err = broker
                .prepare_check(hint, LeanWorkerElabOptions::new())
                .await
                .err()
                .unwrap();
            assert!(matches!(err, ServerError::BadProject(reason) if reason.contains("operator-approved")));
            assert!(broker.resident_paths().is_empty());
        }
        let canonical = broker.resolve(&ProjectHint::Explicit(approved.join("."))).unwrap();
        assert!(broker.runtime_config.require_operator_primary(&canonical).is_ok());
        assert!(
            ProjectRuntimeConfig::default()
                .require_operator_primary(&canonical)
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and matching installed worker"]
    async fn same_project_workers_share_capacity_and_drain_together() {
        let root = PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").unwrap())
            .canonicalize()
            .unwrap();
        let mut config = cfg(root.clone(), Some(root.clone()), None);
        config.max_projects = NonZeroUsize::new(2).unwrap();
        config.max_workers_per_project = NonZeroUsize::new(2).unwrap();
        let broker = ProjectBroker::new(config);
        let (first, _) = broker.acquire(root.clone()).await.unwrap();
        let (second, _) = broker.acquire(root.clone()).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let (third, _) = broker.acquire(root.clone()).await.unwrap();
        assert!(Arc::ptr_eq(&third, &first) || Arc::ptr_eq(&third, &second));
        assert_eq!(broker.worker_slots.available_permits(), 0);
        assert_eq!(broker.resident_paths(), vec![root.clone()]);
        let snapshot = broker.runtime_control(&RuntimeControlRequest::default());
        assert_eq!(snapshot.pool.projects.len(), 2);
        assert_eq!(snapshot.pool.occupied_slots, 2);
        let cancellation = tokio_util::sync::CancellationToken::new();
        cancellation.cancel();
        assert!(
            crate::cancellation::with_cancellation(&cancellation, broker.acquire(root.clone()))
                .await
                .is_err()
        );
        assert!(first.is_healthy() && second.is_healthy());
        let draining = broker.runtime_control(&RuntimeControlRequest {
            action: RuntimeAction::Drain,
            retire_idle: true,
            ..RuntimeControlRequest::default()
        });
        assert_eq!(draining.status, RuntimeControlStatus::Draining);
        assert_eq!(draining.pool.occupied_slots, 2);
        drop((first, second, third));
        let lease_id = draining.lease_id;
        for _ in 0..100 {
            let snapshot = broker.runtime_control(&RuntimeControlRequest {
                action: RuntimeAction::Renew,
                lease_id: lease_id.clone(),
                ..RuntimeControlRequest::default()
            });
            if snapshot.pool.workers_stopped {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("drain did not confirm both worker exits");
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and matching installed worker"]
    async fn same_project_checks_run_on_distinct_workers() {
        let root = PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").unwrap());
        let runtime =
            ProjectRuntimeConfig::from_env_with_file(&crate::config_file::ConfigFile::load(&root).runtime, 2).unwrap();
        let mut config = cfg(root.clone(), Some(root), None);
        config.max_projects = NonZeroUsize::new(2).unwrap();
        config.max_workers_per_project = NonZeroUsize::new(2).unwrap();
        let broker = ProjectBroker::new_with_runtime_config(config, runtime);
        let import = std::env::var("LEAN_HOST_MCP_PARALLEL_IMPORT").unwrap_or_else(|_| "Lean".to_owned());
        let other_import = std::env::var("LEAN_HOST_MCP_PARALLEL_OTHER_IMPORT").unwrap_or_else(|_| import.clone());
        let run = |import: &str| {
            broker.process_module_query(
                ProjectHint::Default,
                vec![import.to_owned()],
                vec![import.to_owned()],
                format!("import {import}\n#eval IO.sleep 1500\nexample : True := True.intro\n"),
                LeanWorkerModuleQuery::Diagnostics,
                LeanWorkerElabOptions::new(),
            )
        };
        let started = Instant::now();
        let (first, second) = tokio::join!(run(&import), run(&other_import));
        let first = first.unwrap();
        let second = second.unwrap();
        for call in [&first, &second] {
            let LeanWorkerModuleQueryOutcome::Ok {
                result: lean_rs_worker_parent::LeanWorkerModuleQueryResult::Diagnostics(messages),
                ..
            } = &call.value
            else {
                panic!("expected diagnostics: {:?}", call.value);
            };
            assert!(
                !messages.diagnostics.iter().any(|message| message.severity == "error"),
                "{messages:?}"
            );
        }
        assert_ne!(first.freshness.session_id, second.freshness.session_id);
        assert_eq!(broker.resident_paths().len(), 1);
        assert_eq!(
            broker
                .runtime_control(&RuntimeControlRequest::default())
                .pool
                .occupied_slots,
            2
        );
        eprintln!(
            "two same-project checks completed in {:?}; sessions {} and {}",
            started.elapsed(),
            first.freshness.session_id,
            second.freshness.session_id
        );
        broker.shutdown_all();
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and matching installed worker"]
    async fn cancelling_one_project_worker_preserves_its_sibling() {
        let root = PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").unwrap());
        let mut config = cfg(root.clone(), Some(root), None);
        config.max_projects = NonZeroUsize::new(2).unwrap();
        config.max_workers_per_project = NonZeroUsize::new(2).unwrap();
        let broker = ProjectBroker::new(config);
        let cancel = tokio_util::sync::CancellationToken::new();
        let run = || {
            broker.process_module_query(
                ProjectHint::Default,
                vec!["Lean".to_owned()],
                vec!["Lean".to_owned()],
                "import Lean\n#eval IO.sleep 1500\nexample : True := True.intro\n".to_owned(),
                LeanWorkerModuleQuery::Diagnostics,
                LeanWorkerElabOptions::new(),
            )
        };
        let first = crate::cancellation::with_cancellation(&cancel, run());
        let stop_first = async {
            for _ in 0..200 {
                let state = broker.runtime_control(&RuntimeControlRequest::default());
                if state.pool.projects.len() == 2 && state.pool.outstanding_jobs >= 2 {
                    cancel.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("two workers never became active");
        };
        let (cancelled, sibling, ()) = tokio::join!(first, run(), stop_first);
        assert!(cancelled.is_err(), "cancellation must not report a successful check");
        let sibling = sibling.unwrap();
        let LeanWorkerModuleQueryOutcome::Ok {
            result: lean_rs_worker_parent::LeanWorkerModuleQueryResult::Diagnostics(messages),
            ..
        } = sibling.value
        else {
            panic!("sibling diagnostics missing");
        };
        assert!(!messages.diagnostics.iter().any(|message| message.severity == "error"));
        broker.shutdown_all();
    }

    #[test]
    fn worker_cap_aliases_respect_environment_precedence() {
        let file = BrokerFileConfig {
            max_projects: Some(4),
            max_workers: Some(3),
            ..BrokerFileConfig::default()
        };
        assert_eq!(parse_worker_pool_config(None, None, None, &file).unwrap().0.get(), 3);
        assert_eq!(
            parse_worker_pool_config(None, Some("2"), None, &file).unwrap().0.get(),
            2
        );
        assert_eq!(
            parse_worker_pool_config(Some("1"), Some("2"), None, &file)
                .unwrap()
                .0
                .get(),
            1
        );
        assert!(parse_worker_pool_config(Some("0"), None, None, &file).is_err());
        assert!(parse_worker_pool_config(Some("bad"), Some("2"), None, &file).is_err());
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
            ..BrokerFileConfig::default()
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
    fn capacity_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now().checked_add(Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn same_root_opening_waiters_preserve_their_original_admission_deadline() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let occupied = broker
            .reserve_worker_slot(Path::new("/occupied"), capacity_deadline())
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(30))
            .unwrap();
        let root = PathBuf::from("/same-cold-project");
        let held = broker.opening_lock(&root).unwrap().lock_owned().await;
        let cancellation = CallCancellation::default();
        let first = broker.acquire_opening_guard(&root, deadline, &cancellation);
        let second = broker.acquire_opening_guard(&root, deadline, &cancellation);
        let last = broker.acquire_opening_guard(&root, deadline, &cancellation);
        tokio::pin!(first, second, last);
        pending(first.as_mut()).await;
        pending(second.as_mut()).await;
        pending(last.as_mut()).await;
        let results = tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(first, second, last) })
            .await
            .unwrap();
        for result in [results.0, results.1, results.2] {
            assert!(
                matches!(&result, Err(ServerError::WorkerUnavailable(info)) if info.reason == "project_admission_timeout")
            );
            drop(result);
        }
        assert_eq!(broker.worker_slots.available_permits(), 0);
        assert!(broker.resident_paths().is_empty());
        drop((occupied, held));
    }

    async fn pending<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn capacity_waiters_are_fifo_and_cancelled_waiters_do_not_steal_slots() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let occupied = broker
            .reserve_worker_slot(Path::new("/occupied"), capacity_deadline())
            .await
            .unwrap();
        let first = broker.reserve_worker_slot(Path::new("/first"), capacity_deadline());
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancelled = crate::cancellation::with_cancellation(
            &cancellation,
            broker.reserve_worker_slot(Path::new("/cancelled"), capacity_deadline()),
        );
        let last = broker.reserve_worker_slot(Path::new("/last"), capacity_deadline());
        tokio::pin!(first, cancelled, last);
        pending(first.as_mut()).await;
        pending(cancelled.as_mut()).await;
        pending(last.as_mut()).await;
        cancellation.cancel();
        assert!(
            matches!(cancelled.await, Err(ServerError::WorkerUnavailable(info)) if info.reason == "request_cancelled")
        );
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(occupied);
        let first_slot = tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap();
        pending(last.as_mut()).await;
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(first_slot);
        let last_slot = tokio::time::timeout(Duration::from_secs(1), last)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(last_slot);
        assert_eq!(broker.worker_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn shutdown_rejects_waiting_capacity_without_releasing_unconfirmed_workers() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let occupied = broker
            .reserve_worker_slot(Path::new("/occupied"), capacity_deadline())
            .await
            .unwrap();
        let waiting = broker.reserve_worker_slot(Path::new("/waiting"), capacity_deadline());
        tokio::pin!(waiting);
        pending(waiting.as_mut()).await;
        broker.shutdown_all();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap();
        assert!(matches!(&result, Err(ServerError::WorkerUnavailable(info)) if info.reason == "project_shutting_down"));
        drop(result);
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(occupied);
    }

    #[tokio::test]
    async fn expired_retirement_wait_preserves_admission_timeout_and_owned_capacity() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let retiring = broker
            .reserve_worker_slot(Path::new("/retiring"), capacity_deadline())
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            broker.wait_for_worker_slot(Path::new("/waiting"), 0, tokio::time::Instant::now()),
        )
        .await
        .unwrap();
        assert!(matches!(&result, Err(ServerError::WorkerUnavailable(info))
            if info.reason == "project_admission_timeout" && info.retryable));
        drop(result);
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(retiring);
        assert_eq!(broker.worker_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn shutdown_interrupts_retirement_wait_without_unconfirmed_slot_release() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let retiring = broker
            .reserve_worker_slot(Path::new("/retiring"), capacity_deadline())
            .await
            .unwrap();
        let waiting = broker.wait_for_worker_slot(Path::new("/waiting"), 0, capacity_deadline());
        tokio::pin!(waiting);
        pending(waiting.as_mut()).await;
        broker.shutdown_all();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap();
        assert!(matches!(&result, Err(ServerError::WorkerUnavailable(info)) if info.reason == "project_shutting_down"));
        drop(result);
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(retiring);
        assert_eq!(broker.worker_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn opening_locks_survive_shutdown_and_a_disappearing_caller() {
        let broker = ProjectBroker::new(cfg(PathBuf::from("/unused"), None, None));
        let root = PathBuf::from("/project");
        let original = broker.opening_lock(&root).unwrap();
        let waiting = Arc::clone(&original);
        broker.shutdown_all();
        drop(original);
        let successor = broker.opening_lock(&root).unwrap();
        assert!(Arc::ptr_eq(&waiting, &successor));
        drop(waiting);
        drop(successor);
        let _other = broker.opening_lock(Path::new("/other")).unwrap();
        assert!(!broker.inner.lock().opening_locks.contains_key(&root));
    }
    #[tokio::test]
    async fn cancelling_capacity_wait_keeps_the_retiring_workers_slot_reserved() {
        let mut config = cfg(PathBuf::from("/unused"), None, None);
        config.max_projects = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let retiring = broker
            .reserve_worker_slot(Path::new("/first"), capacity_deadline())
            .await
            .unwrap();
        let request = tokio_util::sync::CancellationToken::new();
        let cancelled = request.clone();
        let result = crate::cancellation::with_cancellation(&request, async {
            cancelled.cancel();
            broker
                .wait_for_worker_slot(Path::new("/next"), 0, capacity_deadline())
                .await
        })
        .await;
        let reason = match &result {
            Err(ServerError::WorkerUnavailable(info)) => info.reason.clone(),
            _ => panic!("cancelled capacity wait must not acquire a slot"),
        };
        drop(result);
        assert_eq!(reason, "request_cancelled");
        assert_eq!(broker.worker_slots.available_permits(), 0);
        drop(retiring);
        assert_eq!(broker.worker_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_same_project_waiter_does_not_wait_for_startup_or_take_capacity() {
        let broker = ProjectBroker::new(cfg(PathBuf::from("/unused"), None, None));
        let root = PathBuf::from("/blocked-startup");
        let held = broker.opening_lock(&root).unwrap().lock_owned().await;
        let request = tokio_util::sync::CancellationToken::new();
        let cancel = request.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            crate::cancellation::with_cancellation(&request, async {
                cancel.cancel();
                broker
                    .acquire_opening_guard(&root, capacity_deadline(), &CallCancellation::current())
                    .await
            }),
        )
        .await
        .unwrap();
        let reason = match &result {
            Err(ServerError::WorkerUnavailable(info)) => info.reason.clone(),
            _ => panic!("cancelled opening wait must return before startup completes"),
        };
        drop(result);
        assert_eq!(reason, "request_cancelled");
        assert_eq!(
            broker.worker_slots.available_permits(),
            broker.config.max_projects.get()
        );
        drop(held);
    }
}
