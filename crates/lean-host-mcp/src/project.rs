//! `LeanProject`—the unit of multiplexing.
//!
//! Bundles the four resources that share lifetime and invalidation for one
//! Lake project: a closure-channel actor that parks a single
//! [`LeanWorkerCapability`] on a dedicated OS thread, the per-project
//! `DeclarationIndex` (`SQLite`), the per-project [`ProcessedFileCache`]
//! (in-memory LRU), and the project metadata (canonical root, toolchain,
//! package, library, manifest hash, default imports).
//!
//! The actor pattern matches the previous `SessionHost`: one owner of the
//! capability at a time, enforced by parking it on a thread named
//! `"lean-host-mcp/project/<basename>"`. Each call to [`LeanProject::submit`]
//! ships a typed closure to that thread, which opens a fresh session,
//! invokes the worker, and returns a `Send + 'static` result.
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

use lean_rs_worker_parent::{LeanWorkerCapability, LeanWorkerCapabilityBuilder, LeanWorkerChild};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};

use crate::cache::ProcessedFileCache;
use crate::envelope::Freshness;
use crate::error::{Result, ServerError};
use crate::index::DeclarationIndex;
use crate::lake_meta::LakeProjectMeta;
use crate::projections::map_worker_err;
use crate::toolchain::{ToolchainId, WorkerBinary};

/// LRU capacity for the in-memory `ProcessedFile` cache. Sized for a normal
/// multi-file proof session—large enough that twenty cursor moves across
/// a handful of files all hit, small enough to keep memory bounded.
const PROCESSED_FILE_CACHE_CAPACITY: usize = 16;

type Job = Box<dyn FnOnce(&mut LeanWorkerCapability) + Send + 'static>;

/// One Lake project, one worker actor, one in-memory cache, one `SQLite`
/// index. Cheap to clone via `Arc`.
pub struct LeanProject {
    canonical_root: PathBuf,
    /// Raw contents of `<canonical_root>/lean-toolchain`, e.g.
    /// `"leanprover/lean4:v4.30.0-rc2"`. Stored as a string for now; a
    /// parsed `ToolchainId` type will arrive when multi-toolchain dispatch
    /// lands.
    toolchain: String,
    package: String,
    library: String,
    manifest_hash: String,
    default_imports: Vec<String>,
    /// Identity of *this* spawned project actor. Allocated once in
    /// [`Self::open`] and surfaced via [`Freshness::session_id`]. Constant
    /// across every tool call routed to this project; changes only when the
    /// broker evicts and re-spawns. Clients compare against the previous
    /// `session_id` to detect a silent re-spawn (LRU eviction, idle reaper,
    /// manifest invalidation).
    session_id: String,
    actor_tx: Mutex<Option<mpsc::Sender<Job>>>,
    index: Arc<DeclarationIndex>,
    cache: ProcessedFileCache,
}

impl std::fmt::Debug for LeanProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeanProject")
            .field("canonical_root", &self.canonical_root)
            .field("toolchain", &self.toolchain)
            .field("package", &self.package)
            .field("library", &self.library)
            .field("manifest_hash", &self.manifest_hash)
            .field("default_imports", &self.default_imports)
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
    /// capability preflight / handshake failure; `ServerError::Lean` for
    /// runtime open failures; `ServerError::Index` if the cache directory
    /// cannot be created or the `SQLite` database cannot be opened;
    /// `ServerError::Internal` if the OS rejects the actor thread.
    pub fn open(meta: LakeProjectMeta, cache_dir: &Path) -> Result<Arc<Self>> {
        // Resolve the toolchain pin to a concrete worker binary before
        // spawning the actor, so the install error surfaces synchronously
        // and includes the `install-worker` command in its message.
        // TODO(prompt 15): swap this for a structured `NeedsWorker`
        // envelope status rather than embedding the command in a string.
        let toolchain_id =
            ToolchainId::parse(&meta.toolchain).map_err(|e| ServerError::BadProject(e.to_string()))?;
        let worker = WorkerBinary::resolve_for(&toolchain_id)
            .map_err(|e| ServerError::BadProject(e.to_string()))?;
        // Each worker binary is built against one toolchain; its rpath and
        // its `LEAN_SYSROOT` must match. The elan toolchain root is that
        // sysroot. We pass it explicitly to `LeanWorkerChild::for_toolchain`
        // below so the parent can host multiple workers with different
        // toolchains from a single process.
        let lean_sysroot = toolchain_id
            .elan_dir()
            .map_err(|e| ServerError::BadProject(e.to_string()))?;

        let index = Arc::new(DeclarationIndex::open(
            cache_dir,
            &meta.canonical_root.to_string_lossy(),
        )?);

        type InitMsg = std::result::Result<(String, mpsc::Sender<Job>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();

        let lake_root = meta.canonical_root.clone();
        let package = meta.package.clone();
        let library = meta.library.clone();
        let default_imports = meta.default_imports.clone();
        let toolchain_label = meta.toolchain.clone();
        let worker_path = worker.path;
        let thread_name = actor_thread_name(&meta.canonical_root);

        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                actor_main(
                    lake_root,
                    package,
                    library,
                    default_imports,
                    toolchain_label,
                    worker_path,
                    lean_sysroot,
                    init_tx,
                );
            })
            .map_err(|e| ServerError::Internal(format!("spawn project actor thread: {e}")))?;

        let (runtime_toolchain, actor_tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("project actor thread died during init".into()))??;

        // The constant is non-zero by construction; `NonZeroUsize::MIN`
        // ([`NonZeroUsize::new(1)`]) is a safe fallback the type system
        // can verify, so we do not need an `unwrap` here.
        #[allow(
            clippy::missing_const_for_fn,
            reason = "NonZeroUsize::new is const but `or` is not yet on stable for NonZeroUsize"
        )]
        let cache_cap = NonZeroUsize::new(PROCESSED_FILE_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN);

        Ok(Arc::new(Self {
            canonical_root: meta.canonical_root,
            toolchain: runtime_toolchain,
            package: meta.package,
            library: meta.library,
            manifest_hash: meta.manifest_hash,
            default_imports: meta.default_imports,
            session_id: uuid::Uuid::new_v4().to_string(),
            actor_tx: Mutex::new(Some(actor_tx)),
            index,
            cache: ProcessedFileCache::with_capacity(cache_cap),
        }))
    }

    /// Dispatch a closure to the project's worker actor. The closure runs
    /// on the actor thread with exclusive access to the
    /// `LeanWorkerCapability`; its return value is sent back via a
    /// `oneshot`.
    ///
    /// # Errors
    ///
    /// `ServerError::SessionGone` if the actor thread has exited;
    /// otherwise whatever the closure itself returns.
    pub async fn submit<F, R>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&mut LeanWorkerCapability) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let boxed: Job = Box::new(move |cap| {
            let _ = reply_tx.send(job(cap));
        });
        let tx = self
            .actor_tx
            .lock()
            .as_ref()
            .cloned()
            .ok_or(ServerError::SessionGone)?;
        tx.send(boxed).await.map_err(|_| ServerError::SessionGone)?;
        reply_rx.await.map_err(|_| ServerError::SessionGone)?
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn library(&self) -> &str {
        &self.library
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

    pub fn default_imports(&self) -> &[String] {
        &self.default_imports
    }

    pub fn index(&self) -> &DeclarationIndex {
        &self.index
    }

    /// Cheap `Arc` clone for callers that need an owned handle (e.g.
    /// background tasks that outlive the borrow).
    pub fn index_arc(&self) -> Arc<DeclarationIndex> {
        Arc::clone(&self.index)
    }

    pub fn cache(&self) -> &ProcessedFileCache {
        &self.cache
    }

    /// Effective import set for a request: empty input means "use the
    /// project's default imports".
    #[must_use]
    pub fn effective_imports(&self, request_imports: &[String]) -> Vec<String> {
        if request_imports.is_empty() {
            self.default_imports.clone()
        } else {
            request_imports.to_vec()
        }
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
            imports: self.effective_imports(request_imports),
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
        self.actor_tx
            .lock()
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
    }
}

impl Drop for LeanProject {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn actor_thread_name(canonical_root: &Path) -> String {
    let basename = canonical_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    format!("lean-host-mcp/project/{basename}")
}

fn actor_main(
    lake_root: PathBuf,
    package: String,
    library: String,
    default_imports: Vec<String>,
    toolchain_label: String,
    worker_path: PathBuf,
    lean_sysroot: PathBuf,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<Job>), ServerError>>,
) {
    let builder = LeanWorkerCapabilityBuilder::new(&lake_root, &package, &library, default_imports.iter())
        .worker_child(LeanWorkerChild::for_toolchain(worker_path, lean_sysroot))
        .startup_timeout(Duration::from_secs(30))
        // 16 MiB: comfortable headroom for `outline` / `file_diagnostics`
        // on Mathlib-scale modules where a single frame can carry
        // thousands of pretty-printed declarations or diagnostics, with
        // no natural chunking axis the tool layer can exploit. The
        // upstream default (1 MiB) is appropriate for bulk tools that
        // chunk their own pages (`describe_bulk`), not for tools whose
        // single result is the frame.
        .max_frame_bytes(16 * 1024 * 1024)
        .long_running_requests();

    let report = builder.check();
    if let Some(first) = report.first_error() {
        let _ = init_reply.send(Err(ServerError::BadProject(format!(
            "{}: {}",
            first.code(),
            first.message()
        ))));
        return;
    }

    let mut capability = match builder.open() {
        Ok(cap) => cap,
        Err(err) => {
            let _ = init_reply.send(Err(map_worker_err(err)));
            return;
        }
    };

    let runtime_toolchain = capability.runtime_metadata().lean_version.unwrap_or(toolchain_label);

    let (tx, mut rx) = mpsc::channel::<Job>(64);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    while let Some(job) = rx.blocking_recv() {
        job(&mut capability);
    }
}
