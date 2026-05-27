//! `LeanProject`—the unit of multiplexing.
//!
//! Bundles the resources that share lifetime and invalidation for one Lake
//! project: a closure-channel actor that parks a single shims-only
//! [`LeanWorkerHostHandle`] on a dedicated OS thread, the per-project bounded
//! module-query cache, and the project metadata (canonical root, toolchain,
//! package/library hints, manifest hash).
//!
//! `LeanWorkerHostHandle` has one owner at a time; the invariant holds by
//! parking it on a thread named `"lean-host-mcp/project/<basename>"`. Each
//! call to [`LeanProject::submit`] ships a typed closure to that thread,
//! which opens a fresh session, invokes the worker, and returns a
//! `Send + 'static` result.
//!
//! Lean-domain failures (parse, elaboration, kernel rejection, meta
//! timeout) flow as `Ok` payloads through the closure. Only infrastructure
//! failures (worker thread gone, runtime init failed, Lake project
//! unusable) escape as [`ServerError`].

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lean_rs_worker_parent::{
    LeanWorkerChild, LeanWorkerHostHandle, LeanWorkerHostHandleBuilder, LeanWorkerRestartPolicy,
};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::cache::ModuleQueryCache;
use crate::envelope::Freshness;
use crate::error::{Result, ServerError};
use crate::lake_meta::LakeProjectMeta;
use crate::projections::map_worker_err;
use crate::toolchain::{ToolchainId, WorkerBinary};

/// LRU capacity for exact bounded module query results. Query results are
/// intentionally small, so this can hold more cursor probes than the old
/// whole-file cache without creating a large memory resident set.
const MODULE_QUERY_CACHE_CAPACITY: usize = 256;
const WORKER_REQUEST_RESTARTS: u64 = 64;

type Job = Box<dyn FnOnce(&mut LeanWorkerHostHandle) + Send + 'static>;

/// One Lake project, one worker actor, one in-memory cache, one `SQLite`
/// index. Cheap to clone via `Arc`.
pub struct LeanProject {
    canonical_root: PathBuf,
    /// Raw contents of `<canonical_root>/lean-toolchain`, e.g.
    /// `"leanprover/lean4:v4.30.0"`.
    toolchain: String,
    package: Option<String>,
    library: Option<String>,
    manifest_hash: String,
    /// Identity of *this* spawned project actor. Allocated once in
    /// [`Self::open`] and surfaced via [`Freshness::session_id`]. Constant
    /// across every tool call routed to this project; changes only when the
    /// broker evicts and re-spawns. Clients compare against the previous
    /// `session_id` to detect a silent re-spawn (LRU eviction, idle reaper,
    /// manifest invalidation).
    session_id: String,
    actor_tx: Mutex<Option<mpsc::Sender<Job>>>,
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
    /// Spawn the per-project actor thread, start the worker child, open the
    /// Lake project, open the declaration index, and seat the in-memory
    /// cache. Returns a shareable handle.
    ///
    /// # Errors
    ///
    /// `ServerError::BadProject` for unresolvable worker child / failing
    /// shims-only bootstrap / handshake failure; `ServerError::Lean` for
    /// runtime open failures; `ServerError::Index` if the cache directory
    /// cannot be created or the `SQLite` database cannot be opened;
    /// `ServerError::Internal` if the OS rejects the actor thread.
    pub fn open(meta: LakeProjectMeta, _cache_dir: &Path) -> Result<Arc<Self>> {
        // Resolve the toolchain pin to a concrete worker binary before
        // spawning the actor, so the install error surfaces synchronously
        // and includes the `install-worker` command in its message.
        let toolchain_id = ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let worker = WorkerBinary::resolve_for(&toolchain_id).map_err(|e| ServerError::BadProject(e.to_string()))?;
        // Each worker binary is built against one toolchain; its rpath and
        // `LEAN_SYSROOT` must match. The elan toolchain root is that
        // sysroot. Passing it explicitly to `LeanWorkerChild::for_toolchain`
        // lets the parent host multiple workers with different toolchains
        // from a single process.
        let lean_sysroot = toolchain_id
            .elan_dir()
            .map_err(|e| ServerError::BadProject(e.to_string()))?;

        type InitMsg = std::result::Result<(String, mpsc::Sender<Job>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();

        let lake_root = meta.canonical_root.clone();
        let toolchain_label = meta.toolchain.clone();
        let worker_path = worker.path;
        let thread_name = actor_thread_name(&meta.canonical_root);

        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                actor_main(lake_root, toolchain_label, worker_path, lean_sysroot, init_tx);
            })
            .map_err(|e| ServerError::Internal(format!("spawn project actor thread: {e}")))?;

        let (runtime_toolchain, actor_tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("project actor thread died during init".into()))??;

        // Constant is non-zero by construction; `NonZeroUsize::MIN` is a
        // type-checked fallback, so no `unwrap` is needed.
        #[allow(
            clippy::missing_const_for_fn,
            reason = "NonZeroUsize::new is const but `or` is not yet on stable for NonZeroUsize"
        )]
        let cache_cap = NonZeroUsize::new(MODULE_QUERY_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);

        Ok(Arc::new(Self {
            canonical_root: meta.canonical_root,
            toolchain: runtime_toolchain,
            package: meta.package,
            library: meta.library,
            manifest_hash: meta.manifest_hash,
            session_id: uuid::Uuid::new_v4().to_string(),
            actor_tx: Mutex::new(Some(actor_tx)),
            module_queries: ModuleQueryCache::with_capacity(cache_cap),
        }))
    }

    /// Dispatch a closure to the project's worker actor. The closure runs
    /// on the actor thread with exclusive access to the
    /// shims-only host handle; its return value is sent back via a
    /// `oneshot`.
    ///
    /// # Errors
    ///
    /// `ServerError::SessionGone` if the actor thread has exited;
    /// otherwise whatever the closure itself returns.
    pub async fn submit<F, R>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&mut LeanWorkerHostHandle) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed: Job = Box::new(move |cap| {
            let _ = reply_tx.send(job(cap));
        });
        let tx = self.actor_tx.lock().as_ref().cloned().ok_or(ServerError::SessionGone)?;
        tx.send(boxed).await.map_err(|_| ServerError::SessionGone)?;
        let result = reply_rx.await.map_err(|_| ServerError::SessionGone)?;
        if result.as_ref().is_err_and(project_actor_error_is_fatal) {
            self.shutdown();
        }
        result
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

    /// Stable identity of this project actor. See [`Self::session_id`] field
    /// docs for the semantics.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn module_query_cache(&self) -> &ModuleQueryCache {
        &self.module_queries
    }

    /// Build a [`Freshness`] for a request. `session_id` is this project's
    /// stable identity (see [`Self::session_id`]); two calls routed to the
    /// same project see the same value, and it changes only when the broker
    /// evicts and re-spawns.
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

    /// Drop the actor channel; the worker loop's `blocking_recv` will then
    /// return `None` and the thread exits cleanly. Subsequent `submit`
    /// calls return [`ServerError::SessionGone`]. Idempotent.
    pub fn shutdown(&self) {
        let _ = self.actor_tx.lock().take();
    }

    /// Cheap liveness probe for the broker's fast path. `false` when the
    /// actor thread has exited (its receiver dropped) or [`Self::shutdown`]
    /// has been called. Used to evict dead projects from the registry before
    /// every caller has to discover the corpse via `SessionGone`.
    pub fn is_healthy(&self) -> bool {
        self.actor_tx.lock().as_ref().is_some_and(|tx| !tx.is_closed())
    }
}

fn project_actor_error_is_fatal(err: &ServerError) -> bool {
    match err {
        ServerError::BadProject(message) | ServerError::Lean(message) => {
            message.contains("ChildPanicOrAbort")
                || message.contains("ChildExited")
                || message.contains("child exited")
                || message.contains("worker exited")
                || message.contains("worker protocol")
        }
        ServerError::SessionGone => true,
        ServerError::Index(_) | ServerError::Io(_) | ServerError::Internal(_) => false,
    }
}

impl Drop for LeanProject {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn actor_thread_name(canonical_root: &Path) -> String {
    let basename = canonical_root.file_name().and_then(|s| s.to_str()).unwrap_or("project");
    format!("lean-host-mcp/project/{basename}")
}

fn actor_main(
    lake_root: PathBuf,
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<Job>), ServerError>>,
) {
    let builder = LeanWorkerHostHandleBuilder::shims_only(&lake_root, std::iter::empty::<String>())
        .worker_child(LeanWorkerChild::for_toolchain(worker_path, lean_sysroot))
        .startup_timeout(Duration::from_secs(30))
        .long_running_requests()
        .restart_policy(LeanWorkerRestartPolicy::default().max_requests(WORKER_REQUEST_RESTARTS));

    let report = builder.check();
    if let Some(first) = report.first_error() {
        let _ = init_reply.send(Err(ServerError::BadProject(format!(
            "{}: {}",
            first.code(),
            first.message()
        ))));
        return;
    }

    let mut handle = match builder.open() {
        Ok(handle) => handle,
        Err(err) => {
            let _ = init_reply.send(Err(map_worker_err(err)));
            return;
        }
    };

    let runtime_toolchain = handle.runtime_metadata().lean_version.unwrap_or(toolchain_label);

    let (tx, mut rx) = mpsc::channel::<Job>(64);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    while let Some(job) = rx.blocking_recv() {
        job(&mut handle);
    }
}
