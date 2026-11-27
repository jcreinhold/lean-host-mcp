//! The server store: a private directory for server state, and the lock that
//! admits one server per store.
//!
//! The lock is an exclusive `flock` on `<store>/server.lock`, held for the
//! server's lifetime. The kernel drops it when the process exits, however it
//! exits, so a crashed server never leaves the store locked. The holder writes
//! its pid and Lake root into the same file; a second server that cannot take
//! the lock reads them to name the server that holds it.
//!
//! The store also keeps the state that would otherwise go under `HOME`: the
//! SDK owner directory `owners/`, the semantic runtime cache
//! `semantic-runtimes/` and the server process records `processes/`. A server
//! or install selects it for this with [`use_for_native_state`].

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use lean_toolchain::native_guardian::{OWNERS_DIRECTORY_ENV, bind_registry_directory};

use crate::config_file::{self, ConfigFile};

/// Env var that overrides the store directory for every Lake root. It beats
/// the config files' `[install] store_dir`.
pub const STORE_DIR_ENV: &str = "LEAN_HOST_MCP_STORE_DIR";

/// The lock file inside the store.
const LOCK_FILE_NAME: &str = "server.lock";

/// The SDK owner directory inside the store, unless
/// `LEAN_RS_WORKER_OWNERS_DIRECTORY` names another.
pub const OWNERS_DIR_NAME: &str = "owners";

/// The semantic runtime cache inside the store, unless
/// `LEAN_HOST_MCP_SEMANTIC_CACHE_DIR` names another.
pub const SEMANTIC_CACHE_DIR_NAME: &str = "semantic-runtimes";

/// The server process records inside the store, unless
/// `LEAN_HOST_MCP_PROCESS_REGISTRY_DIR` names another directory.
pub const PROCESSES_DIR_NAME: &str = "processes";

/// The canonical store this process keeps its native state in, set by
/// [`use_for_native_state`].
static NATIVE_STORE: OnceLock<PathBuf> = OnceLock::new();

/// Keep this process's native state in the existing store `store`.
///
/// It binds the SDK owner directory `<store>/owners`, unless
/// `LEAN_RS_WORKER_OWNERS_DIRECTORY` names another, and makes
/// `<store>/semantic-runtimes` the semantic runtime cache. Call it once,
/// before any native work. It creates neither directory; the SDK creates the
/// owner directory with the mode it checks. Returns the canonical store.
///
/// # Errors
///
/// Fails when `store` cannot be resolved, when an earlier call chose another
/// store, or when the SDK refuses the owner directory.
pub fn use_for_native_state(store: &Path) -> io::Result<PathBuf> {
    let store = store.canonicalize()?;
    if *NATIVE_STORE.get_or_init(|| store.clone()) != store {
        return Err(io::Error::other(
            "this process already keeps its native state in another store",
        ));
    }
    owner_directory()?;
    Ok(store)
}

/// The canonical store set by [`use_for_native_state`], if any.
#[must_use]
pub fn native_store() -> Option<&'static Path> {
    NATIVE_STORE.get().map(PathBuf::as_path)
}

/// Bind the SDK to the owner directory of this process's workers and writers,
/// and return it.
///
/// That is `LEAN_RS_WORKER_OWNERS_DIRECTORY`, else `owners` in the store set
/// by [`use_for_native_state`]. The host never falls back to the SDK's
/// directory under the user's data directory. Binding creates nothing: the SDK
/// creates the directory, with the mode it checks.
///
/// # Errors
///
/// Fails when neither names a directory, or when the SDK refuses it.
pub fn owner_directory() -> io::Result<PathBuf> {
    let directory = if let Some(path) = std::env::var_os(OWNERS_DIRECTORY_ENV) {
        PathBuf::from(path)
    } else if let Some(store) = native_store() {
        store.join(OWNERS_DIR_NAME)
    } else {
        return Err(io::Error::other(format!(
            "no native owner directory: serve or install with --lake-root, or set {OWNERS_DIRECTORY_ENV}"
        )));
    };
    bind_registry_directory(&directory)?;
    Ok(directory)
}

/// The store directory for Lake root `root`: `flag` (a CLI flag), then
/// `LEAN_HOST_MCP_STORE_DIR`, then `[install] store_dir` from the root's config
/// files, then `<root>/.local/lean-host-mcp/store`.
#[must_use]
pub fn store_dir(root: &Path, flag: Option<&Path>) -> PathBuf {
    store_dir_with(root, flag, std::env::var_os(STORE_DIR_ENV))
}

/// [`store_dir`] with the env var's value passed in.
pub(crate) fn store_dir_with(root: &Path, flag: Option<&Path>, env: Option<OsString>) -> PathBuf {
    config_file::pick_dir(
        flag,
        env,
        || ConfigFile::load(Some(root)).install.store_dir(root),
        || config_file::local_dir(root).join("store"),
    )
}

/// Create the store directory `store` for Lake root `root` with mode 0700,
/// and tighten an existing one to 0700. A store under the root's local
/// directory gets that directory's `.gitignore` first.
///
/// # Errors
///
/// Returns the I/O error when a directory cannot be created or its mode set.
pub fn create_store_dir(store: &Path, root: &Path) -> io::Result<()> {
    if store.starts_with(config_file::local_dir(root)) {
        config_file::ensure_local_dir(root)?;
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(store)?;
    fs::set_permissions(store, fs::Permissions::from_mode(0o700))
}

/// The store lock of a running server. Dropping it frees the store.
#[derive(Debug)]
pub struct StoreLock {
    file: File,
    path: PathBuf,
}

impl StoreLock {
    /// Create `store` if needed and lock it for the server of Lake root
    /// `root`, recording this process's pid and the root.
    ///
    /// # Errors
    ///
    /// Fails when another process holds the lock, naming its pid and Lake
    /// root, or when the store cannot be created or written.
    pub fn acquire(store: &Path, root: &Path) -> anyhow::Result<Self> {
        create_store_dir(store, root).map_err(|e| anyhow::anyhow!("cannot create store {}: {e}", store.display()))?;
        let path = store.join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("cannot open store lock {}: {e}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(held_by_another(&mut file, store)),
            Err(TryLockError::Error(e)) => {
                return Err(anyhow::anyhow!("cannot lock store {}: {e}", store.display()));
            }
        }
        let lock = Self { file, path };
        lock.record(root)
            .map_err(|e| anyhow::anyhow!("cannot write store lock {}: {e}", lock.path.display()))?;
        Ok(lock)
    }

    fn record(&self, root: &Path) -> io::Result<()> {
        let mut file = &self.file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        write!(file, "pid={}\nroot={}\n", std::process::id(), root.display())?;
        file.sync_data()
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // A forked child may share the open file description until it execs,
        // so end the lock explicitly instead of waiting for the last close.
        if let Err(error) = self.file.unlock() {
            tracing::warn!(%error, path = %self.path.display(), "failed to unlock the store");
        }
    }
}

/// The refusal for a store another server holds, naming that server from
/// what it wrote into the lock file.
fn held_by_another(file: &mut File, store: &Path) -> anyhow::Error {
    let mut text = String::new();
    // An unreadable or half-written record still refuses; it only loses the
    // holder's name.
    if let Err(error) = file.read_to_string(&mut text) {
        tracing::debug!(%error, "cannot read the store lock record");
    }
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    match (field("pid"), field("root")) {
        (Some(pid), Some(root)) => anyhow::anyhow!(
            "another lean-host-mcp (pid {pid}, lake root {root}) holds the store {}; stop it first",
            store.display()
        ),
        _ => anyhow::anyhow!(
            "another lean-host-mcp holds the store {}; stop it first",
            store.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_names_the_holder() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("project");
        let store = config_file::local_dir(&root).join("store");
        fs::create_dir(&root)?;

        let held = StoreLock::acquire(&store, &root)?;
        // flock conflicts between open file descriptions, even in one process.
        let refusal = StoreLock::acquire(&store, &root).err().map(|e| e.to_string());
        let expected = format!(
            "another lean-host-mcp (pid {}, lake root {}) holds the store {}; stop it first",
            std::process::id(),
            root.display(),
            store.display()
        );
        assert_eq!(refusal, Some(expected));

        drop(held);
        let again = StoreLock::acquire(&store, &root);
        assert!(again.is_ok(), "a dropped lock frees the store");
        Ok(())
    }

    #[test]
    fn store_is_private_and_ignored() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("project");
        fs::create_dir(&root)?;
        let store = config_file::local_dir(&root).join("store");
        create_store_dir(&store, &root)?;
        let mode = fs::metadata(&store)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(config_file::local_dir(&root).join(".gitignore").is_file());
        Ok(())
    }

    #[test]
    fn empty_record_still_refuses() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store = tmp.path().join("store");
        fs::create_dir(&store)?;
        let holder = File::create(store.join(LOCK_FILE_NAME))?;
        holder.try_lock()?;
        let refusal = StoreLock::acquire(&store, tmp.path()).err().map(|e| e.to_string());
        let expected = format!(
            "another lean-host-mcp holds the store {}; stop it first",
            store.display()
        );
        assert_eq!(refusal, Some(expected));
        Ok(())
    }
}
