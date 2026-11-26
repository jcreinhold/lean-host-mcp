//! Parent-only artifact reads own close-on-exec shared descriptors until parsing ends.
//!
//! These descriptors are never loaned to a child and close atomically on exec.
//! A concurrent fork can inherit them until exec; native readers instead need the
//! guardian loan protocol. All cooperating readers and writers acquire
//! the complete workspace union in the same canonical lock-path order.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const READ_WAIT: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(5);

/// Lexically complete custody for one parent-side artifact read, closed on drop.
pub(crate) struct ArtifactReadGuard {
    roots: Vec<PathBuf>,
    _locks: Vec<File>,
}

impl ArtifactReadGuard {
    pub(crate) fn for_project(root: &Path) -> io::Result<Self> {
        Self::acquire(root, READ_WAIT)
    }

    #[cfg(unix)]
    pub(crate) fn acquire(root: &Path, timeout: Duration) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        let roots = lean_toolchain::shared_writer::project_artifact_roots(root)?;
        let mut paths = roots
            .iter()
            .map(|root| lean_toolchain::shared_writer::project_lock_path(root))
            .collect::<io::Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| invalid("artifact read deadline is too large"))?;
        let mut locks = Vec::with_capacity(paths.len());
        for path in &paths {
            // open(2) atomically sets CLOEXEC and refuses a symlink. There is no
            // descriptor-receive window in this multithreaded parent process.
            let file = File::from(rustix::fs::open(
                path,
                rustix::fs::OFlags::RDWR
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )?);
            loop {
                match file.try_lock_shared() {
                    Ok(()) => {}
                    Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(POLL);
                        continue;
                    }
                    Err(TryLockError::WouldBlock) => return Err(busy()),
                    Err(TryLockError::Error(error)) => return Err(error),
                }
                let opened = file.metadata()?;
                let named = std::fs::symlink_metadata(path)?;
                if !opened.is_file()
                    || opened.nlink() != 1
                    || !named.is_file()
                    || (opened.dev(), opened.ino()) != (named.dev(), named.ino())
                {
                    return Err(invalid("artifact read lock identity changed"));
                }
                // A writer helper may have died while its command group still
                // runs. SH alone cannot establish that this prior writer ended.
                // Native readers are compatible with SH and are fenced at EX.
                if lean_toolchain::shared_writer::previous_writer_stopped(&file)? {
                    break;
                }
                file.unlock()?;
                if Instant::now() >= deadline {
                    return Err(busy());
                }
                std::thread::sleep(POLL);
            }
            locks.push(file);
        }
        if lean_toolchain::shared_writer::project_artifact_roots(root)? != roots {
            return Err(invalid("artifact dependency domains changed during read admission"));
        }
        Ok(Self { roots, _locks: locks })
    }

    #[cfg(not(unix))]
    pub(crate) fn acquire(_root: &Path, _timeout: Duration) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "artifact read custody requires Unix",
        ))
    }

    /// Artifact search paths must not escape into an unprotected `LEAN_PATH` or
    /// through a build-directory symlink. Missing build paths remain admissible.
    pub(crate) fn covers_artifact_root(&self, path: &Path) -> bool {
        resolve_missing(path).is_ok_and(|path| {
            path.ancestors()
                .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".lake"))
                .and_then(Path::parent)
                .is_some_and(|workspace| self.roots.iter().any(|root| root == workspace))
        })
    }
}

fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| invalid("artifact path has no stable ancestor"))?;
                missing.push(name);
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("artifact path has no parent"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn busy() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "artifact read custody is busy or a previous writer remains live",
    )
}

#[cfg(test)]
mod tests;
