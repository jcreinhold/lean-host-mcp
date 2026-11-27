//! The semantic-search runtime each toolchain needs, built once per process
//! through the server's writer helper, and the directory that caches it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lean_semantic_search_runtime::SemanticSearchRuntimeBuild;
use parking_lot::Mutex;

use crate::cancellation::CallCancellation;
use crate::error::{Result, ServerError};

/// Built semantic-search runtimes, one per `(toolchain label, Lean sysroot)`.
///
/// `lean_semantic_search_runtime::build_cached_with_writer` is cached *on disk*, not in
/// process: every call re-materializes the source package, takes a filesystem
/// build lock, and runs a Lake build to conclude there is nothing to do.
/// Measured warm on this machine at **5.5 ms**, paid on the project's actor
/// thread — where it blocks the worker — on every semantic call. The result is
/// a pure function of the key, so it is computed once per process instead.
///
/// Process-wide rather than per worker because that is the true scope: every
/// worker of the served project, and each one that replaces it, needs the
/// identical runtime from the identical cache directory.
///
/// The build itself runs *outside* the lock. Holding it would serialize every
/// worker behind one Lake invocation; upstream's own filesystem lock already
/// makes a concurrent duplicate build safe, and the loser simply overwrites an
/// equal value.
static SEMANTIC_RUNTIMES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<(String, PathBuf), lean_semantic_search_runtime::SemanticSearchRuntime>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Preserve writer evidence until the parent resolves this operation. A
/// diagnostic string alone cannot distinguish no result from uncertain cleanup.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct SemanticRuntimeFailure {
    message: String,
    writer: Option<lean_toolchain::shared_writer::WriterFailure>,
}

impl SemanticRuntimeFailure {
    fn from_error(error: impl std::error::Error + 'static) -> Self {
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
        let mut writer = None;
        while let Some(current) = source {
            writer = current
                .downcast_ref::<lean_toolchain::shared_writer::WriterFailure>()
                .cloned()
                .or_else(|| {
                    current
                        .downcast_ref::<std::io::Error>()
                        .and_then(lean_toolchain::shared_writer::WriterFailure::from_io)
                        .cloned()
                });
            if writer.is_some() {
                break;
            }
            source = current.source();
        }
        Self {
            message: error.to_string(),
            writer,
        }
    }

    /// Whether the writer reported that its command never started or stopped.
    pub(crate) fn closed(&self) -> bool {
        matches!(
            self.writer.as_ref().map(|failure| failure.disposition),
            Some(
                lean_toolchain::shared_writer::WriterDisposition::NotStarted
                    | lean_toolchain::shared_writer::WriterDisposition::Stopped
            )
        )
    }
}

/// Memoized supervised semantic runtime build; failures are never cached.
pub(crate) fn semantic_runtime(
    toolchain_label: &str,
    lean_sysroot: &Path,
    cancellation: &CallCancellation,
) -> std::result::Result<lean_semantic_search_runtime::SemanticSearchRuntime, SemanticRuntimeFailure> {
    if cancellation.is_cancelled() {
        return Err(SemanticRuntimeFailure::from_error(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            lean_toolchain::shared_writer::WriterFailure {
                disposition: lean_toolchain::shared_writer::WriterDisposition::NotStarted,
                detail: "semantic runtime preparation cancelled before admission".to_owned(),
                stop_receipt: None,
            },
        )));
    }
    let key = (toolchain_label.to_owned(), lean_sysroot.to_path_buf());
    // Bound the guard to a statement so the lock is released before the build
    // below, rather than living to the end of an `if let`.
    let cached = SEMANTIC_RUNTIMES.lock().get(&key).cloned();
    if let Some(runtime) = cached {
        return Ok(runtime);
    }
    let cache_root = semantic_runtime_cache_root().map_err(SemanticRuntimeFailure::from_error)?;
    // This build runs in the parent, so a child-only environment binding cannot
    // provide writer custody. Select this executable's helper explicitly.
    let writer = lean_rs_writer::contract().map_err(SemanticRuntimeFailure::from_error)?;
    let runtime = lean_semantic_search_runtime::build_cached_with_writer(
        SemanticSearchRuntimeBuild {
            cache_root,
            toolchain_label: toolchain_label.to_owned(),
            lean_sysroot: lean_sysroot.to_path_buf(),
        },
        &writer.executable,
        Duration::from_secs(120),
        || cancellation.is_cancelled(),
    )
    .map_err(SemanticRuntimeFailure::from_error)?;
    SEMANTIC_RUNTIMES.lock().insert(key, runtime.clone());
    Ok(runtime)
}

/// The semantic runtime cache: `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`, a canonical
/// path created if missing, else `semantic-runtimes` in the server's store.
fn semantic_runtime_cache_root() -> Result<PathBuf> {
    // This changes only the destination of the released runtime builder. It
    // still checks the package inputs and uses the same supervised writer.
    if let Some(dir) = std::env::var_os("LEAN_HOST_MCP_SEMANTIC_CACHE_DIR") {
        return env_cache_dir(PathBuf::from(dir));
    }
    crate::store::native_store()
        .map(|store| store.join(crate::store::SEMANTIC_CACHE_DIR_NAME))
        .ok_or_else(|| {
            ServerError::BadProject(
                "no semantic runtime cache: serve with --lake-root, or set LEAN_HOST_MCP_SEMANTIC_CACHE_DIR".into(),
            )
        })
}

/// Create `dir` if it is missing and check that it is canonical, naming the
/// variable and the path in each failure.
fn env_cache_dir(dir: PathBuf) -> Result<PathBuf> {
    const VAR: &str = "LEAN_HOST_MCP_SEMANTIC_CACHE_DIR";
    let not_canonical =
        || ServerError::BadProject(format!("{VAR} must be a canonical absolute path: {}", dir.display()));
    if !dir.is_absolute() {
        return Err(not_canonical());
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| ServerError::BadProject(format!("cannot create {VAR} {}: {error}", dir.display())))?;
    if dir.canonicalize().ok().as_ref() != Some(&dir) {
        return Err(not_canonical());
    }
    Ok(dir)
}

#[cfg(test)]
#[path = "semantic_preparation_tests.rs"]
mod semantic_preparation_tests;
