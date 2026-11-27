//! The semantic-search runtime each toolchain needs, and the directory that
//! caches it.
//!
//! Requirement: nothing loads the runtime's modules while Lake writes them.
//! `install --lake-root` builds the runtime into the store's cache while it
//! holds the store, so no server runs. A serving process only opens it, which
//! builds nothing, and keeps what it opened.

use std::path::{Path, PathBuf};

use lean_semantic_search_runtime::{SemanticSearchRuntime, SemanticSearchRuntimeBuild};
use parking_lot::Mutex;

use crate::error::{Result, ServerError};

/// Opened semantic-search runtimes, one per `(toolchain label, Lean sysroot)`.
///
/// Opening asks Lake whether the modules are current, which starts a Lake
/// process. While the server serves only install builds, so the answer cannot
/// change and each process opens a key once. The open runs outside the lock;
/// two callers that race open the same runtime and the second stores an equal
/// value.
static SEMANTIC_RUNTIMES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<(String, PathBuf), SemanticSearchRuntime>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// The runtime for `toolchain_label` and `lean_sysroot`, opened once per
/// process. Failures are not kept, so a later call after an install opens it.
///
/// # Errors
///
/// When the runtime is not built, tells the operator to rerun install;
/// otherwise returns the cache or Lake failure.
pub(crate) fn semantic_runtime(toolchain_label: &str, lean_sysroot: &Path) -> Result<SemanticSearchRuntime> {
    let key = (toolchain_label.to_owned(), lean_sysroot.to_path_buf());
    // Bound the guard to a statement so the lock is released before the open
    // below, rather than living to the end of an `if let`.
    let cached = SEMANTIC_RUNTIMES.lock().get(&key).cloned();
    if let Some(runtime) = cached {
        return Ok(runtime);
    }
    let runtime = open_runtime(semantic_runtime_cache_root()?, toolchain_label, lean_sysroot)?;
    SEMANTIC_RUNTIMES.lock().insert(key, runtime.clone());
    Ok(runtime)
}

/// Open the runtime built in `cache_root`, without building it.
fn open_runtime(cache_root: PathBuf, toolchain_label: &str, lean_sysroot: &Path) -> Result<SemanticSearchRuntime> {
    lean_semantic_search_runtime::open(SemanticSearchRuntimeBuild {
        cache_root,
        toolchain_label: toolchain_label.to_owned(),
        lean_sysroot: lean_sysroot.to_path_buf(),
    })
    .map_err(|error| {
        if error.needs_build() {
            ServerError::BadProject(format!(
                "the semantic search runtime for {toolchain_label} is not built; rerun \
                 `lean-host-mcp install --lake-root <root>`"
            ))
        } else {
            ServerError::BadProject(format!(
                "cannot open the semantic search runtime for {toolchain_label}: {error}"
            ))
        }
    })
}

/// Build the runtime for `toolchain_label` and `lean_sysroot` into
/// `cache_root`, or the cache that `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` names.
/// Install calls this while it holds the store.
///
/// # Errors
///
/// Returns the cache, materialization or Lake failure.
pub(crate) fn build_semantic_runtime(
    cache_root: &Path,
    toolchain_label: &str,
    lean_sysroot: &Path,
) -> Result<SemanticSearchRuntime> {
    let cache_root = match std::env::var_os(CACHE_DIR_ENV) {
        Some(dir) => env_cache_dir(PathBuf::from(dir))?,
        None => cache_root.to_path_buf(),
    };
    lean_semantic_search_runtime::build(
        SemanticSearchRuntimeBuild {
            cache_root,
            toolchain_label: toolchain_label.to_owned(),
            lean_sysroot: lean_sysroot.to_path_buf(),
        },
        || false,
    )
    .map_err(|error| {
        ServerError::BadProject(format!(
            "cannot build the semantic search runtime for {toolchain_label}: {error}"
        ))
    })
}

/// The variable that names another semantic runtime cache.
const CACHE_DIR_ENV: &str = "LEAN_HOST_MCP_SEMANTIC_CACHE_DIR";

/// The semantic runtime cache: `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR`, a canonical
/// path created if missing, else `semantic-runtimes` in the server's store.
fn semantic_runtime_cache_root() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CACHE_DIR_ENV) {
        return env_cache_dir(PathBuf::from(dir));
    }
    crate::store::process_store()
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
    let not_canonical = || {
        ServerError::BadProject(format!(
            "{CACHE_DIR_ENV} must be a canonical absolute path: {}",
            dir.display()
        ))
    };
    if !dir.is_absolute() {
        return Err(not_canonical());
    }
    std::fs::create_dir_all(&dir).map_err(|error| {
        ServerError::BadProject(format!("cannot create {CACHE_DIR_ENV} {}: {error}", dir.display()))
    })?;
    if dir.canonicalize().ok().as_ref() != Some(&dir) {
        return Err(not_canonical());
    }
    Ok(dir)
}

#[cfg(test)]
#[path = "semantic_preparation_tests.rs"]
mod semantic_preparation_tests;
