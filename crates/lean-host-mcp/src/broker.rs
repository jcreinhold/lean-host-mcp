//! `ProjectBroker`: the mediator between MCP tool dispatch and the workers of
//! the one Lake project a server serves.
//!
//! Two responsibilities:
//!
//! 1. **Resolve** a tool call's [`ProjectHint`] to the served root,
//!    [`BrokerConfig::lake_root`]. A call that names another root fails, and
//!    so does every call when the broker serves no root.
//! 2. **Dispatch** typed semantic operations to up to
//!    [`BrokerConfig::max_workers_per_project`] workers of that root, opening
//!    one on first use, reusing it, and retiring it when idle.
//!
//! Tool modules call narrow domain methods such as
//! [`ProjectBroker::inspect_declaration`] and never receive raw worker handles.
//!
//! **Idle retirement.** A background reaper spawned from
//! [`ProjectBroker::new`] runs every 60 s and retires workers idle longer than
//! [`BrokerConfig::idle_timeout`] (default 600 s; [`Duration::ZERO`] turns
//! this off).
//!
//! **Manifest invalidation.** Every reuse compares the project's
//! `lake-manifest.json` hash and treats a mismatch as a miss: the worker shuts
//! down and a new one starts. The hash comes from `ProjectBroker::meta`, which
//! rebuilds [`LakeProjectMeta`] only when the files it derives from have
//! changed, so a call costs a few `stat` calls rather than a manifest hash.
//!
//! **Startup.** The registry mutex is never held across worker spawn or
//! command execution. Concurrent misses serialize startup and recheck the
//! worker limit.
//!
//! **Admission.** Each worker owns a serialized thread with a bounded
//! mailbox. [`BrokerConfig::max_workers`] slots count resident, opening and
//! retiring workers until each exit is confirmed;
//! [`ProjectBroker::runtime_status`] observes the pool.
//!
//! **Short runs.** A `lean` or `lake` process the server starts for a check
//! (a file check, a `lake setup-file` or `lake query` run, or a semantic file run)
//! first takes one of [`BrokerConfig::max_lean_runs`] permits. Requirement: at
//! most that many short runs execute at once, whatever the number of clients.

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
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::{ModuleQueryBatchKey, ModuleQueryKey};
use crate::cancellation::CallCancellation;
use crate::config_file::{self, BrokerFileConfig, Env};
use crate::envelope::{Freshness, RuntimeFacts};
use crate::error::{Result, ServerError};
use crate::lake_meta::{LakeProjectMeta, ProjectInputStamp};
use crate::project::{LeanProject, ProjectCall, ProjectRuntimeConfig};

mod control;
mod host_artifacts;
pub(crate) mod module_setup;
mod reader;
pub(crate) mod semantic_file;
pub(crate) mod verify_file;
pub(crate) use control::MaintenanceAccess;
pub use control::{RuntimeStatus, WorkerActivity, WorkerPoolStatus};

/// Default worker slots when `LEAN_HOST_MCP_MAX_WORKERS` is unset.
pub const DEFAULT_MAX_WORKERS: usize = 4;

/// Default short-run permits when `LEAN_HOST_MCP_MAX_LEAN_RUNS` is unset.
pub const DEFAULT_MAX_LEAN_RUNS: usize = 2;

/// Default idle window (10 min) when `LEAN_HOST_MCP_IDLE_TIMEOUT_SECS` is
/// unset.
///
/// Long enough that an interactive session keeps its worker warm, short
/// enough that a forgotten session releases its worker child soon.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;

const REAPER_TICK: Duration = Duration::from_mins(1);
/// How often a build checks whether the workers it stopped have exited.
const WORKER_EXIT_POLL: Duration = Duration::from_millis(25);

/// Broker inputs, built once at startup from the CLI, env and config files.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// The one Lake root this broker serves, from `--lake-root` /
    /// `LEAN_HOST_MCP_PROJECT`. Calls on any other root are refused, and so
    /// is every call when it is unset.
    pub lake_root: Option<PathBuf>,
    /// Worker slots, counting workers that are starting or still exiting.
    pub max_workers: NonZeroUsize,
    /// Workers that may serve the project at once.
    pub max_workers_per_project: NonZeroUsize,
    /// Short `lean` and `lake` runs that may execute at once.
    pub max_lean_runs: NonZeroUsize,
    /// Idle window after which the reaper retires a worker.
    /// [`Duration::ZERO`] turns idle retirement off.
    pub idle_timeout: Duration,
}

impl BrokerConfig {
    /// A broker serving `lake_root` with the default pool: one worker, the
    /// default slots, the default short-run permits and the default idle
    /// window.
    #[must_use]
    pub fn serving(lake_root: PathBuf) -> Self {
        Self {
            lake_root: Some(lake_root),
            ..Self::unserved()
        }
    }

    /// A broker that serves no root, with the default pool.
    #[must_use]
    pub fn unserved() -> Self {
        Self {
            lake_root: None,
            max_workers: NonZeroUsize::new(DEFAULT_MAX_WORKERS).unwrap_or(NonZeroUsize::MIN),
            max_workers_per_project: NonZeroUsize::MIN,
            max_lean_runs: NonZeroUsize::new(DEFAULT_MAX_LEAN_RUNS).unwrap_or(NonZeroUsize::MIN),
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
        }
    }

    /// Read the pool limits of a broker serving `lake_root`. Each limit is
    /// `env var > config file > default`.
    ///
    /// # Errors
    ///
    /// [`ServerError::Internal`] when an env var does not parse or a worker
    /// or short-run limit is zero. An idle timeout of zero turns idle retirement off.
    pub fn from_env_with_file(lake_root: Option<PathBuf>, file: &BrokerFileConfig) -> Result<Self> {
        parse_pool_config(lake_root, &config_file::process_env, file)
    }
}

/// [`BrokerConfig::from_env_with_file`] with the env lookup passed in.
fn parse_pool_config(lake_root: Option<PathBuf>, env: Env<'_>, file: &BrokerFileConfig) -> Result<BrokerConfig> {
    let positive = |name: &str, file: Option<usize>, default: usize| {
        config_file::positive_setting(env, name, file, default)
            .and_then(|value| NonZeroUsize::new(value).ok_or_else(|| format!("{name} is 0")))
            .map_err(ServerError::Internal)
    };
    let idle_timeout_secs = config_file::setting(env, "LEAN_HOST_MCP_IDLE_TIMEOUT_SECS", file.idle_timeout_secs)
        .map_err(ServerError::Internal)?
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    Ok(BrokerConfig {
        lake_root,
        max_workers: positive("LEAN_HOST_MCP_MAX_WORKERS", file.max_workers, DEFAULT_MAX_WORKERS)?,
        max_workers_per_project: positive("LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT", file.max_workers_per_project, 1)?,
        max_lean_runs: positive("LEAN_HOST_MCP_MAX_LEAN_RUNS", file.max_lean_runs, DEFAULT_MAX_LEAN_RUNS)?,
        idle_timeout: Duration::from_secs(idle_timeout_secs),
    })
}

/// A call's `project` field. `Default` selects the served root; `Explicit`
/// must name it.
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
    registry: HashMap<WorkerKey, Arc<LeanProject>>,
    last_used: HashMap<WorkerKey, Instant>,
    opening_locks: HashMap<PathBuf, Weak<AsyncMutex<()>>>,
    shutdown_epoch: u64,
    /// Last-built [`LakeProjectMeta`], with the stamp of the files it came
    /// from. Purely a cost cache: [`ProjectBroker::meta`] revalidates against
    /// disk on every lookup, so it is never served stale.
    meta_cache: Option<(ProjectInputStamp, LakeProjectMeta)>,
    /// Fair read and write turns per canonical root.
    locks: crate::project_lock::ProjectLocks,
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
    /// One permit per short run; see the module docs.
    lean_runs: Arc<Semaphore>,
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
    /// ([`tokio::runtime::Handle::try_current`]). It still collects retired
    /// workers when `idle_timeout` is zero; only idle expiry is off.
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
                registry: HashMap::new(),
                last_used: HashMap::new(),
                opening_locks: HashMap::new(),
                shutdown_epoch: 0,
                meta_cache: None,
                locks: crate::project_lock::ProjectLocks::default(),
            })),
            runtime_config,
            worker_slots: Arc::new(Semaphore::new(config.max_workers.get())),
            lean_runs: Arc::new(Semaphore::new(config.max_lean_runs.get())),
            capacity_admission: AsyncMutex::new(()),
            config,
        });
        broker.spawn_reaper();
        broker
    }

    #[cfg(test)]
    pub(crate) fn test_register_actor(&self, project: Arc<LeanProject>) {
        let root = project.canonical_root().to_path_buf();
        let mut inner = self.inner.lock();
        // As after `acquire`, the actor's root has a project lock.
        inner.locks.get(&root);
        inner.registry.insert(WorkerKey { root, lane: 0 }, project);
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
            max_workers: self.config.max_workers.get(),
            max_workers_per_project: self.config.max_workers_per_project.get(),
            idle_timeout_secs: self.config.idle_timeout.as_secs(),
            request_timeout_millis: self.runtime_config.request_timeout_millis(),
            lean_max_memory_kib: self.runtime_config.lean_max_memory_kib(),
        }
    }

    /// The permits every short run takes before it starts.
    pub(crate) fn lean_runs(&self) -> Arc<Semaphore> {
        Arc::clone(&self.lean_runs)
    }

    fn spawn_reaper(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak: Weak<Self> = Arc::downgrade(self);
        handle.spawn(async move {
            let mut idle = tokio::time::interval(REAPER_TICK);
            idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            idle.tick().await;
            loop {
                idle.tick().await;
                let Some(broker) = weak.upgrade() else {
                    break;
                };
                // Retirement stays off the async executor.
                if let Err(error) = tokio::task::spawn_blocking(move || broker.reap_idle()).await {
                    tracing::warn!(%error, "idle reaper failed; the next tick tries again");
                }
            }
        });
    }

    /// The canonical served root, which `hint` must select.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::BadProject`] when the broker serves no root, a
    /// path does not canonicalise, or `hint` names another root.
    pub fn resolve(&self, hint: &ProjectHint) -> Result<PathBuf> {
        let Some(served) = &self.config.lake_root else {
            return Err(ServerError::BadProject(
                "this server serves no Lake project; start it with `--lake-root`".into(),
            ));
        };
        let served = canonicalise(served)?;
        if let ProjectHint::Explicit(path) = hint {
            let root = canonicalise(path)?;
            if root != served {
                return Err(ServerError::BadProject(format!(
                    "this server serves {}, not {}; start a server with `--lake-root` set to that project",
                    served.display(),
                    root.display()
                )));
            }
        }
        Ok(served)
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
        // Revalidate outside the lock: the stamp is a few `stat` calls, and
        // the registry mutex also guards the worker pool.
        let cached = self.inner.lock().meta_cache.clone();
        if let Some((stamp, meta)) = cached
            && meta.canonical_root == root
            && meta.input_stamp() == stamp
        {
            return Ok(meta);
        }
        let meta = LakeProjectMeta::from_explicit(root)?;
        let stamp = meta.input_stamp();
        self.inner.lock().meta_cache = Some((stamp, meta.clone()));
        Ok(meta)
    }

    /// `result`, with a failure to load an import that names no module told
    /// as [`ServerError::UnknownImports`]; see `name_unknown_imports`.
    async fn name_imports<T>(&self, root: &Path, imports: &[String], result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(err) => Err(self.name_unknown_imports(root, imports, err).await),
        }
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
            .process_module_query(session_imports.clone(), source, query, options, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
        // The request already holds its read turn; this reads its count.
        let builds_finished = self.ensure_read(project.canonical_root()).await?;
        let context =
            crate::cache::ModuleQueryContext::capture(&session_imports, &freshness_imports, &options, builds_finished);
        // The lookup is a plain in-process map probe on the project handle: a
        // hit answers without ever entering the actor's mailbox, so a warm
        // repeat of an unmodified file never queues behind in-flight Lean work.
        if let Some(value) = key.as_ref().zip(context.as_ref()).and_then(|(key, context)| {
            project
                .module_query_cache()
                .get(&path, content_hash, context, key, project.live_generation())
        }) {
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
            .process_module_query(session_imports.clone(), source, query, options, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
            .await?;
        let (value, runtime) = call.into_parts();
        if let Some(((key, context), generation)) = key.zip(context).zip(crate::cache::completed_generation(&runtime)) {
            project
                .module_query_cache()
                .insert(path, content_hash, context, key, generation, value.clone());
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
            .process_module_query_batch(
                session_imports.clone(),
                source,
                selectors,
                budgets,
                options,
                admission_deadline,
            )
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
        let builds_finished = self.ensure_read(project.canonical_root()).await?;
        let context =
            crate::cache::ModuleQueryContext::capture(&session_imports, &freshness_imports, &options, builds_finished);
        // As in `process_cached_module_query`: a hit bypasses the mailbox, and
        // an unkeyable batch runs uncached.
        if let Some(value) = key.as_ref().zip(context.as_ref()).and_then(|(key, context)| {
            project
                .module_query_cache()
                .get_batch(&path, content_hash, context, key, project.live_generation())
        }) {
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
            .process_module_query_batch(
                session_imports.clone(),
                source,
                selectors,
                budgets,
                options,
                admission_deadline,
            )
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
            .await?;
        let (value, runtime) = call.into_parts();
        if let Some(((key, context), generation)) = key.zip(context).zip(crate::cache::completed_generation(&runtime)) {
            project
                .module_query_cache()
                .insert_batch(path, content_hash, context, key, generation, value.clone());
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
            .inspect_declaration(session_imports.clone(), request, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
            .search_declarations(session_imports.clone(), requests, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
            .attempt_proof(session_imports.clone(), request, options, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
            .verify_declaration(session_imports.clone(), request, options, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
            .await?;
        let mut out = broker_call(&project, &freshness_imports, call);
        out.freshness.elaboration_options = Some(option_facts);
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
        let (project, options, admission_deadline) = self.prepare_check(hint, options).await?;
        let option_facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let call = project
            .verify_declaration_batch(session_imports.clone(), request, options, admission_deadline)
            .await;
        let call = self
            .name_imports(project.canonical_root(), &session_imports, call)
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
    /// operation evidence still determines the call's disposition.
    async fn prepare_check(
        &self,
        hint: ProjectHint,
        options: LeanWorkerElabOptions,
    ) -> Result<(Arc<LeanProject>, LeanWorkerElabOptions, Instant)> {
        let file_label = options.file_label.clone();
        let admission = async {
            let root = self.resolve(&hint)?;
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
        // Find the worker before Lake runs the project's configuration. This
        // reads metadata only, so a missing worker comes first. The lookup
        // takes the request's read turn.
        let _worker = crate::project::resolve_worker(meta)?;
        let lean_options = self.module_options(&meta.canonical_root, &options.file_label).await?;
        Ok(options.lean_options(lean_options))
    }

    async fn project_for(&self, hint: ProjectHint) -> Result<(Arc<LeanProject>, Instant)> {
        let root = self.resolve(&hint)?;
        self.acquire(root).await
    }

    /// The current request's deadline, or the configured request timeout from
    /// now outside a bounded request.
    pub(crate) fn request_deadline(&self) -> Result<tokio::time::Instant> {
        CallCancellation::current()
            .deadline_or(Duration::from_millis(self.request_timeout_millis()))
            .ok_or_else(|| ServerError::Internal("request deadline is out of range".to_owned()))
    }

    /// Give this request its read turn on `root` before it pins a worker or
    /// starts a short run. The turn lasts until the request and every short
    /// run it started end. Returns the finished-build count at the grant.
    pub(crate) async fn ensure_read(&self, root: &Path) -> Result<u64> {
        let lock = self.project_lock(root);
        let deadline = self.request_deadline()?;
        crate::project_lock::ensure_read(&lock, root, deadline, &CallCancellation::current())
            .await
            .map_err(|refused| Self::turn_refused(root, refused))
    }

    /// The turn lock of canonical project root `root`.
    pub(crate) fn project_lock(&self, root: &Path) -> Arc<crate::project_lock::ProjectLock> {
        self.inner.lock().locks.get(root)
    }

    /// The error a request on `root` gets when it gets no turn.
    pub(crate) fn turn_refused(root: &Path, refused: crate::project_lock::TurnRefused) -> ServerError {
        use crate::project_lock::TurnRefused;
        match refused {
            TurnRefused::Expired { blocked_by } => {
                Self::pool_unavailable_behind(root, "project_admission_timeout", true, blocked_by)
            }
            TurnRefused::Cancelled => Self::pool_unavailable(root, "request_cancelled", false),
            TurnRefused::OtherRoot(held) => {
                ServerError::Internal(format!("request already holds a project turn on {}", held.display()))
            }
            TurnRefused::HoldsRead => {
                ServerError::Internal("a request that holds a read turn asked to write".to_owned())
            }
        }
    }

    /// Look up or open the project for `root`. Mutex is released around
    /// the slow path so concurrent calls for different roots parallelize. The
    /// request deadline continues through the returned actor's queue admission.
    async fn acquire(&self, root: PathBuf) -> Result<(Arc<LeanProject>, Instant)> {
        self.ensure_read(&root).await?;
        let deadline = self.request_deadline()?;
        let project = self.acquire_until(root, deadline).await?;
        Ok((project, deadline.into_std()))
    }

    async fn acquire_until(&self, root: PathBuf, deadline: tokio::time::Instant) -> Result<Arc<LeanProject>> {
        if CallCancellation::current().is_cancelled() {
            return Err(Self::pool_unavailable(&root, "request_cancelled", false));
        }
        let epoch = self.inner.lock().shutdown_epoch;
        if let Some(project) = self.select_worker(&root)? {
            return Ok(project);
        }

        // Read the installed shims before retaining an opening Arc or slot.
        let meta_root = root.clone();
        let meta = tokio::task::spawn_blocking(move || LakeProjectMeta::from_explicit(&meta_root))
            .await
            .map_err(|err| ServerError::Internal(format!("project metadata task failed: {err}")))??;
        let (worker, sysroot, _) = crate::project::resolve_worker(&meta)?;
        let cancellation = CallCancellation::current();
        let prepared =
            host_artifacts::installed_host_artifacts(&root, &worker, &sysroot, deadline, &cancellation).await?;
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
                    !inner.registry.contains_key(&WorkerKey {
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
        // Extra workers are opportunistic: never wait for capacity while an
        // existing worker can accept work.
        let slot = if growing {
            let Ok(_turn) = self.capacity_admission.try_lock() else {
                return self
                    .select_existing_worker(&root)
                    .ok_or_else(|| Self::pool_unavailable(&root, "worker_pool_busy", true));
            };
            match Arc::clone(&self.worker_slots).try_acquire_owned() {
                Ok(slot) => slot,
                Err(_) => {
                    return self
                        .select_existing_worker(&root)
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
        let root = root.clone();
        // The blocking startup keeps running if this async caller disappears.
        // The actor owns initialization evidence independently of this waiter.
        let initialization = cancellation.begin_operation();
        let opened = tokio::task::spawn_blocking(move || {
            // Hold same-root serialization through registration even when the
            // async caller disappears while blocking startup is still running.
            let _open_guard = open_guard;
            let opened = LeanProject::open(meta, runtime_config, slot, maintenance, prepared, initialization)?;
            let mut registry = inner.lock();
            if registry.shutdown_epoch != epoch {
                drop(registry);
                opened.shutdown();
                return Err(Self::pool_unavailable(&root, "project_shutting_down", false));
            }
            registry.registry.insert(key.clone(), Arc::clone(&opened));
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
        let open_lock = self.opening_lock(root);
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
        if !self.inner.lock().registry.iter().any(|(key, _)| key.root == root) {
            return Ok(None);
        }
        let manifest_hash = self.meta(root)?.manifest_hash;
        let mut inner = self.inner.lock();
        let stale: Vec<_> = inner
            .registry
            .iter()
            .filter(|(key, actor)| key.root == root && (!actor.is_healthy() || actor.manifest_hash() != manifest_hash))
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(actor) = inner.registry.remove(&key) {
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

    fn select_existing_worker(&self, root: &Path) -> Option<Arc<LeanProject>> {
        let mut inner = self.inner.lock();
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
        let key = key?;
        inner.last_used.insert(key.clone(), Instant::now());
        inner.registry.get(&key).cloned()
    }

    fn opening_lock(&self, root: &Path) -> Arc<AsyncMutex<()>> {
        let mut inner = self.inner.lock();
        inner.opening_locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = inner.opening_locks.get(root).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        inner.opening_locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    async fn reserve_worker_slot(&self, root: &Path, deadline: tokio::time::Instant) -> Result<OwnedSemaphorePermit> {
        crate::worker_retirement::poll_retirements();
        let cancellation = CallCancellation::current();
        if cancellation.is_cancelled() {
            return Err(Self::pool_unavailable(root, "request_cancelled", false));
        }
        let epoch = self.inner.lock().shutdown_epoch;
        // One FIFO queue owns slot acquisition: a later caller cannot take a
        // released slot from an earlier waiter. The project has no resident
        // worker here, so only workers still exiting hold slots.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(Self::pool_unavailable(root, "request_cancelled", false)),
            result = tokio::time::timeout_at(deadline, async {
                let _turn = self.capacity_admission.lock().await;
                self.wait_for_worker_slot(root, epoch, deadline).await
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
        Self::pool_unavailable_behind(root, reason, retryable, None)
    }

    /// `blocked_by` names the build that a request waited behind, if known.
    fn pool_unavailable_behind(root: &Path, reason: &str, retryable: bool, blocked_by: Option<String>) -> ServerError {
        ServerError::worker_unavailable(crate::error::WorkerUnavailable {
            blocked_by,
            retryable,
            worker_restarted: false,
            worker_stop_receipt: None,
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

    /// Retire each idle, unborrowed worker last used more than
    /// `config.idle_timeout` ago. Called every `REAPER_TICK` by the
    /// background task; tests call this directly to avoid waiting.
    pub fn reap_idle(&self) {
        crate::worker_retirement::poll_retirements();
        if self.config.idle_timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        let evicted: Vec<Arc<LeanProject>> = {
            let mut inner = self.inner.lock();
            let expired: Vec<WorkerKey> = inner
                .last_used
                .iter()
                .filter(|(_, last)| now.saturating_duration_since(**last) >= self.config.idle_timeout)
                .map(|(p, _)| p.clone())
                .collect();
            let mut out: Vec<Arc<LeanProject>> = Vec::with_capacity(expired.len());
            for p in &expired {
                if let Some(proj) = inner.registry.get(p)
                    && (!proj.is_idle() || Arc::strong_count(proj) != 1)
                {
                    continue;
                }
                if let Some(proj) = inner.registry.remove(p) {
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

    /// Stop every worker of canonical Lake root `root` and wait until each has
    /// exited. The caller holds the root's write turn, so no check holds one of
    /// these workers and none can start another. The next check on `root`
    /// starts a fresh worker.
    ///
    /// # Errors
    /// Fails when a worker has not exited by `deadline`.
    pub(crate) async fn stop_workers(&self, root: &Path, deadline: tokio::time::Instant) -> Result<()> {
        let stopped = self.remove_workers(root);
        for project in &stopped {
            project.shutdown();
        }
        while !stopped.iter().all(|project| project.has_exited()) {
            if tokio::time::Instant::now() >= deadline {
                return Err(ServerError::Internal(format!(
                    "a worker of {} did not exit before the build deadline",
                    root.display()
                )));
            }
            tokio::time::sleep(WORKER_EXIT_POLL).await;
        }
        // Release the stopped workers' slots now rather than on the next tick.
        crate::worker_retirement::poll_retirements();
        Ok(())
    }

    /// Take every worker of `root` out of the registry.
    fn remove_workers(&self, root: &Path) -> Vec<Arc<LeanProject>> {
        let mut inner = self.inner.lock();
        let keys: Vec<WorkerKey> = inner
            .registry
            .iter()
            .filter(|(key, _)| key.root == root)
            .map(|(key, _)| key.clone())
            .collect();
        let mut removed = Vec::with_capacity(keys.len());
        for key in &keys {
            inner.last_used.remove(key);
            removed.extend(inner.registry.remove(key));
        }
        drop(inner);
        removed
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
            let projects: Vec<_> = inner.registry.drain().map(|(_, project)| project).collect();
            inner.last_used.clear();
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

    /// The roots that have a resident worker. Public so integration tests can
    /// assert retirement without a tool call.
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
    pub max_workers: usize,
    pub max_workers_per_project: usize,
    pub idle_timeout_secs: u64,
    pub request_timeout_millis: u64,
    /// The Lean heap ceiling each worker child runs under. Reported because it
    /// is the worker's allocator backstop: exceeding it can abort the child.
    /// This is not a process-footprint or machine-wide memory ceiling.
    pub lean_max_memory_kib: u64,
}

/// Freshness of a reply that elaborated nothing.
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

/// Freshness of a reply that a short run elaborated. Each run is one `lean`
/// process, so each has its own session id.
fn short_run_freshness(meta: &LakeProjectMeta, imports: Vec<String>) -> Freshness {
    Freshness {
        session_id: format!("short-run-{}", uuid::Uuid::new_v4()),
        ..freshness_from_meta(meta, imports)
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

    /// The fixture root of an ignored test that starts workers.
    pub(super) fn worker_fixture() -> PathBuf {
        PathBuf::from(std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").unwrap())
            .canonicalize()
            .unwrap()
    }

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

    fn cfg(lake_root: Option<PathBuf>) -> BrokerConfig {
        BrokerConfig {
            lake_root,
            idle_timeout: Duration::ZERO,
            ..BrokerConfig::unserved()
        }
    }

    #[tokio::test]
    async fn native_calls_refuse_every_root_but_the_served_one_before_options() {
        let tree = tempfile::tempdir().unwrap();
        let served = tree.path().join("served");
        let other = tree.path().join("other");
        fs::create_dir(&served).unwrap();
        fs::create_dir(&other).unwrap();
        // This marker permits discovery. There is deliberately no Lean toolchain.
        fs::write(other.join("lakefile.lean"), "").unwrap();
        let canonical = served.canonicalize().unwrap();
        let broker = ProjectBroker::new(cfg(Some(served.clone())));
        let err = broker
            .prepare_check(ProjectHint::Explicit(other.clone()), LeanWorkerElabOptions::new())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&err, ServerError::BadProject(reason) if reason.contains(&format!("this server serves {}", canonical.display()))),
            "{err:?}"
        );
        assert!(broker.resident_paths().is_empty());
        assert_eq!(broker.resolve(&ProjectHint::Default).unwrap(), canonical);
        assert_eq!(
            broker.resolve(&ProjectHint::Explicit(served.join("."))).unwrap(),
            canonical
        );
        // A broker that serves no root refuses every call, even one naming a
        // directory with a lakefile.
        let unserved = ProjectBroker::new(cfg(None));
        for hint in [ProjectHint::Default, ProjectHint::Explicit(other)] {
            let err = unserved
                .prepare_check(hint, LeanWorkerElabOptions::new())
                .await
                .err()
                .unwrap();
            assert!(
                matches!(&err, ServerError::BadProject(reason) if reason.contains("serves no Lake project")),
                "{err:?}"
            );
        }
        assert!(unserved.resident_paths().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and a matching installed worker"]
    async fn same_project_workers_share_capacity_and_stop_together() {
        let root = worker_fixture();
        let mut config = cfg(Some(root.clone()));
        config.max_workers = NonZeroUsize::new(2).unwrap();
        config.max_workers_per_project = NonZeroUsize::new(2).unwrap();
        let broker = ProjectBroker::new(config);
        let (first, _) = broker.acquire(root.clone()).await.unwrap();
        let (second, _) = broker.acquire(root.clone()).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let (third, _) = broker.acquire(root.clone()).await.unwrap();
        assert!(Arc::ptr_eq(&third, &first) || Arc::ptr_eq(&third, &second));
        assert_eq!(broker.worker_slots.available_permits(), 0);
        assert_eq!(broker.resident_paths(), vec![root.clone()]);
        let snapshot = broker.runtime_status();
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
        drop((first, second, third));
        broker.shutdown_all();
        for _ in 0..100 {
            if broker.runtime_status().pool.workers_stopped {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("shutdown did not confirm both worker exits");
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and a matching installed worker"]
    async fn same_project_checks_run_on_distinct_workers() {
        let root = worker_fixture();
        let runtime = ProjectRuntimeConfig::from_env_with_file(
            &crate::config_file::ConfigFile::load(Some(&root)).unwrap().runtime,
            2,
        )
        .unwrap();
        let mut config = cfg(Some(root));
        config.max_workers = NonZeroUsize::new(2).unwrap();
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
        assert_eq!(broker.runtime_status().pool.occupied_slots, 2);
        eprintln!(
            "two same-project checks completed in {:?}; sessions {} and {}",
            started.elapsed(),
            first.freshness.session_id,
            second.freshness.session_id
        );
        broker.shutdown_all();
    }

    #[tokio::test]
    #[ignore = "requires LEAN_HOST_MCP_TEST_FIXTURE and a matching installed worker"]
    async fn cancelling_one_project_worker_preserves_its_sibling() {
        let root = worker_fixture();
        let mut config = cfg(Some(root));
        config.max_workers = NonZeroUsize::new(2).unwrap();
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
                let state = broker.runtime_status();
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

    /// An env lookup that sets the pool's worker, per-project and idle vars.
    fn env(
        max_workers: Option<&'static str>,
        per_project: Option<&'static str>,
        idle: Option<&'static str>,
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| {
            match name {
                "LEAN_HOST_MCP_MAX_WORKERS" => max_workers,
                "LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT" => per_project,
                "LEAN_HOST_MCP_IDLE_TIMEOUT_SECS" => idle,
                _ => None,
            }
            .map(std::ffi::OsString::from)
        }
    }

    #[test]
    fn worker_limits_take_env_over_file_over_default() {
        let file = BrokerFileConfig {
            max_workers: Some(3),
            max_workers_per_project: Some(2),
            ..BrokerFileConfig::default()
        };
        let limits = |env: Env<'_>| {
            let config = parse_pool_config(None, env, &file).unwrap();
            (config.max_workers.get(), config.max_workers_per_project.get())
        };
        assert_eq!(limits(&env(None, None, None)), (3, 2));
        assert_eq!(limits(&env(Some("1"), Some("1"), None)), (1, 1));
        // An empty env var counts as unset.
        assert_eq!(limits(&env(Some(""), Some(" "), None)), (3, 2));
        assert!(parse_pool_config(None, &env(None, Some("0"), None), &file).is_err());
        assert!(parse_pool_config(None, &env(None, Some("bad"), None), &file).is_err());
    }

    #[test]
    fn resolve_canonicalises_a_path_to_the_served_root() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "explicit");
        let resolved = ProjectBroker::new(cfg(Some(proj.clone())))
            .resolve(&ProjectHint::Explicit(proj.join(".")))
            .unwrap();
        assert_eq!(resolved, proj);
    }

    #[test]
    fn parse_pool_config_uses_defaults_when_unset() {
        let empty = BrokerFileConfig::default();
        let config = parse_pool_config(None, &|_| None, &empty).unwrap();
        assert_eq!(config.max_workers.get(), DEFAULT_MAX_WORKERS);
        assert_eq!(config.max_workers_per_project, NonZeroUsize::MIN);
        assert_eq!(config.idle_timeout, Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS));
    }

    #[test]
    fn parse_pool_config_accepts_explicit_values() {
        let empty = BrokerFileConfig::default();
        let config = parse_pool_config(None, &env(Some("8"), None, Some("30")), &empty).unwrap();
        assert_eq!(config.max_workers.get(), 8);
        assert_eq!(config.idle_timeout, Duration::from_secs(30));
    }

    #[test]
    fn shutdown_all_without_resident_projects_is_idempotent() {
        let broker = ProjectBroker::new(cfg(None));
        broker.shutdown_all();
        broker.shutdown_all();
        assert!(broker.resident_paths().is_empty());
        drop(broker);
    }

    #[test]
    fn identity_without_worker_does_not_open_resident_project() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "identity");
        let broker = ProjectBroker::new(cfg(Some(proj.clone())));

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

    /// A short run's reply names the run in its session id, one id per run.
    #[test]
    fn short_run_replies_name_a_session_of_their_own() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = LakeProjectMeta::from_explicit(&make_lake_dir(tmp.path(), "short-run")).unwrap();
        let first = short_run_freshness(&meta, Vec::new());
        let second = short_run_freshness(&meta, Vec::new());
        assert!(first.session_id.starts_with("short-run-"), "{}", first.session_id);
        assert_ne!(first.session_id, second.session_id);
        assert_eq!(first.project_root, meta.canonical_root.to_string_lossy());
    }

    /// The metadata memo is a cost cache, so the contract is that no caller can
    /// tell it is there. Edit an input and the very next lookup must reflect the
    /// edit — the manifest hash gates project respawn, so a stale one silently
    /// pins a server to a dependency set the project has moved off of.
    #[test]
    fn resolve_meta_reflects_a_manifest_edit_on_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = make_lake_dir(tmp.path(), "memo");
        let broker = ProjectBroker::new(cfg(Some(proj.clone())));
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
        let broker = ProjectBroker::new(cfg(Some(proj.clone())));
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
        let config = parse_pool_config(None, &env(None, None, Some("0")), &empty).unwrap();
        assert_eq!(config.idle_timeout, Duration::ZERO);
    }

    #[test]
    fn parse_pool_config_rejects_zero_max_workers() {
        let empty = BrokerFileConfig::default();
        let err = parse_pool_config(None, &env(Some("0"), None, None), &empty).unwrap_err();
        assert!(matches!(err, ServerError::Internal(_)), "{err:?}");
    }

    #[test]
    fn parse_pool_config_rejects_garbage() {
        let e = BrokerFileConfig::default();
        assert!(parse_pool_config(None, &env(Some("seven"), None, None), &e).is_err());
        assert!(parse_pool_config(None, &env(None, None, Some("forever")), &e).is_err());
    }

    #[test]
    fn parse_pool_config_file_value_used_when_env_unset_and_env_wins() {
        let file = BrokerFileConfig {
            max_workers: Some(7),
            idle_timeout_secs: Some(45),
            ..BrokerFileConfig::default()
        };
        // Env unset -> file values used.
        let config = parse_pool_config(None, &|_| None, &file).unwrap();
        assert_eq!(config.max_workers.get(), 7);
        assert_eq!(config.idle_timeout, Duration::from_secs(45));
        // Env present -> env wins over the file value.
        let config = parse_pool_config(None, &env(Some("3"), None, Some("30")), &file).unwrap();
        assert_eq!(config.max_workers.get(), 3);
        assert_eq!(config.idle_timeout, Duration::from_secs(30));
    }

    #[test]
    fn parse_pool_config_rejects_zero_max_workers_from_file() {
        let file = BrokerFileConfig {
            max_workers: Some(0),
            ..BrokerFileConfig::default()
        };
        assert!(parse_pool_config(None, &|_| None, &file).is_err());
    }
    fn capacity_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now().checked_add(Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn same_root_opening_waiters_preserve_their_original_admission_deadline() {
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
        let broker = ProjectBroker::new(config);
        let occupied = broker
            .reserve_worker_slot(Path::new("/occupied"), capacity_deadline())
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_millis(30))
            .unwrap();
        let root = PathBuf::from("/same-cold-project");
        let held = broker.opening_lock(&root).lock_owned().await;
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
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
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
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
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
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
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
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
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
        let broker = ProjectBroker::new(cfg(None));
        let root = PathBuf::from("/project");
        let original = broker.opening_lock(&root);
        let waiting = Arc::clone(&original);
        broker.shutdown_all();
        drop(original);
        let successor = broker.opening_lock(&root);
        assert!(Arc::ptr_eq(&waiting, &successor));
        drop(waiting);
        drop(successor);
        let _other = broker.opening_lock(Path::new("/other"));
        assert!(!broker.inner.lock().opening_locks.contains_key(&root));
    }
    #[tokio::test]
    async fn cancelling_capacity_wait_keeps_the_retiring_workers_slot_reserved() {
        let mut config = cfg(None);
        config.max_workers = NonZeroUsize::MIN;
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
        let broker = ProjectBroker::new(cfg(None));
        let root = PathBuf::from("/blocked-startup");
        let held = broker.opening_lock(&root).lock_owned().await;
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
        assert_eq!(broker.worker_slots.available_permits(), broker.config.max_workers.get());
        drop(held);
    }
}
