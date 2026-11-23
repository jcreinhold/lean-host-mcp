//! Supported launcher selection; this directory lock carries no job authority.
use std::{
    fs::File,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};
use serde::{Deserialize, Serialize};

const CONFIG: &str = "lean-host-mcp.toml";

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LauncherMode {
    Ordinary,
    Managed,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    mode: LauncherMode,
    generation: u64,
    managed_store: PathBuf,
    managed_store_uuid: String,
}

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

fn lock_project(root: &Path, lock: FlockOperation) -> io::Result<File> {
    if !root.is_absolute() || root.canonicalize()? != root {
        return Err(invalid("launcher project must be canonical and absolute"));
    }
    let file = File::from(open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    flock(&file, lock)?;
    Ok(file)
}

fn read_config(root: &Path) -> io::Result<(toml::Table, u32)> {
    let path = root.join(CONFIG);
    let file = File::from(open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o022 != 0
    {
        return Err(invalid("launcher selector requires an owned regular project config"));
    }
    let mut text = String::new();
    file.take(65537).read_to_string(&mut text)?;
    if text.len() > 65536 {
        return Err(invalid("launcher selector config exceeds limit"));
    }
    let table: toml::Table = toml::from_str(&text).map_err(io::Error::other)?;
    Ok((table, meta.mode()))
}

fn load(root: &Path) -> io::Result<(toml::Table, Selection, u32)> {
    let (table, permissions) = read_config(root)?;
    let selection: Selection = table
        .get("launcher")
        .cloned()
        .ok_or_else(|| invalid("project launcher selection missing"))?
        .try_into()
        .map_err(io::Error::other)?;
    if uuid::Uuid::parse_str(&selection.managed_store_uuid).is_err()
        || selection.generation == 0
        || !selection.managed_store.is_absolute()
        || selection.managed_store.canonicalize()? != selection.managed_store
    {
        return Err(invalid("invalid launcher generation or original store"));
    }
    Ok((table, selection, permissions))
}

/// Recheck the generation after taking a shared lock and keep it for the server lifetime.
///
/// # Errors
/// Returns an error if a transition holds the lock or startup selected stale inputs.
pub fn admit(root: &Path, mode: LauncherMode, generation: u64, store: Option<&Path>) -> io::Result<(File, [u8; 16])> {
    // A warm enrolment changes the original store schema. Keep the
    // selector exclusive so a cold broker cannot have that store open.
    let operation = FlockOperation::NonBlockingLockShared;
    #[cfg(lean_host_mcp_development_warm)]
    let operation = if mode == LauncherMode::Managed && crate::managed_prepared::warm_development_enabled() {
        FlockOperation::NonBlockingLockExclusive
    } else {
        operation
    };
    let held = lock_project(root, operation)?;
    let (_, selected, _) = load(root)?;
    if selected.mode != mode
        || selected.generation != generation
        || match mode {
            LauncherMode::Ordinary => store.is_some(),
            LauncherMode::Managed => store != Some(selected.managed_store.as_path()),
        }
    {
        return Err(invalid("launcher selection or generation changed"));
    }
    held.sync_all()?;
    let expected = uuid::Uuid::parse_str(&selected.managed_store_uuid).map_err(io::Error::other)?;
    if crate::native_store::store::NativeStore::launcher_store_identity(&selected.managed_store)?
        != *expected.as_bytes()
    {
        return Err(invalid("original launcher store UUID changed"));
    }
    Ok((held, *expected.as_bytes()))
}

/// Keep an unenrolled stock launcher running only while its project remains unenrolled.
///
/// # Errors
/// Rejects an enrolled, unreadable or malformed project configuration.
pub fn admit_unenrolled(root: &Path) -> io::Result<File> {
    let guard = lock_project(root, FlockOperation::NonBlockingLockShared)?;
    match read_config(root) {
        Ok((table, _)) if !table.contains_key("launcher") => (),
        Ok(_) => {
            return Err(invalid(
                "enrolled project requires --launcher-mode and --launcher-generation",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    Ok(guard)
}

/// Find the nearest enclosing project config when startup does not name a project.
pub fn enclosing_config_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors()
        .find(|path| path.join(CONFIG).symlink_metadata().is_ok())
}

/// Fixed guarded selector transition, enabled only in the exact local SDK qualification build.
#[derive(Debug, clap::Args)]
pub struct Transition {
    #[arg(long)]
    project: PathBuf,
    #[arg(long)]
    from_generation: u64,
    #[arg(long)]
    to: LauncherMode,
    /// JSON path with the original registry identity and exact lock inode union.
    #[arg(long)]
    custody: PathBuf,
}

impl Transition {
    /// Update selectors only, while physical and durable admission are fenced.
    ///
    /// # Errors
    /// Refuses an unavailable guard, active launcher, stale selection or unresolved stage.
    pub fn run(&self) -> io::Result<()> {
        use lean_toolchain::native_guardian::writer_storage::{StorageRoot, with_drained_project};
        use std::{fs, io::Write as _, os::unix::fs::PermissionsExt as _};

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Custody {
            registry: StorageRoot,
            locks: Vec<(PathBuf, u64, u64)>,
        }

        let selector_lock = lock_project(&self.project, FlockOperation::NonBlockingLockExclusive)?;
        let (mut table, mut selected, permissions) = load(&self.project)?;
        let next = self
            .from_generation
            .checked_add(1)
            .filter(|_| self.from_generation != 0)
            .ok_or_else(|| invalid("invalid launcher transition generation"))?;
        let reconcile = selected.generation == next && selected.mode == self.to;
        if !reconcile && (selected.generation != self.from_generation || selected.mode == self.to) {
            return Err(invalid("launcher transition source changed"));
        }
        let proof = File::from(open(
            &self.custody,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if !proof.metadata()?.is_file() {
            return Err(invalid("custody pins must be a regular file"));
        }
        let mut bytes = Vec::new();
        proof.take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            return Err(invalid("custody pins exceed limit"));
        }
        let pins: Custody = serde_json::from_slice(&bytes)?;
        let original_store = selected.managed_store.clone();
        let expected_store = uuid::Uuid::parse_str(&selected.managed_store_uuid).map_err(io::Error::other)?;
        with_drained_project(
            &pins.registry,
            std::slice::from_ref(&self.project),
            &pins.locks,
            std::time::Duration::from_secs(3),
            || {
                crate::native_store::store::NativeStore::with_settled_launcher_store(
                    &original_store,
                    *expected_store.as_bytes(),
                    || {
                        if reconcile {
                            return selector_lock.sync_all();
                        }
                        selected.mode = self.to;
                        selected.generation = next;
                        table.insert(
                            "launcher".into(),
                            toml::Value::try_from(&selected).map_err(io::Error::other)?,
                        );
                        let mut replacement = tempfile::NamedTempFile::new_in(&self.project)?;
                        replacement
                            .as_file()
                            .set_permissions(fs::Permissions::from_mode(permissions & 0o777))?;
                        replacement.write_all(toml::to_string(&table).map_err(io::Error::other)?.as_bytes())?;
                        replacement.as_file().sync_all()?;
                        replacement
                            .persist(self.project.join(CONFIG))
                            .map_err(io::Error::other)?;
                        if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
                            && std::env::var_os("LEAN_HOST_MCP_TEST_LAUNCHER_RENAME_CUT").is_some()
                        {
                            return Err(invalid("disposable cut after selector rename"));
                        }
                        selector_lock.sync_all()
                    },
                )
            },
        )
    }
}
