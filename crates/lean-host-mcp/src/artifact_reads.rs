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

    /// Hash only the selected Lake provenance files while this guard owns SH custody.
    #[cfg(all(unix, test))]
    pub(crate) fn lake_trace_snapshot(
        &self,
        root: &Path,
        targets: &[String],
        mut check_deadline: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<std::collections::BTreeMap<String, String>>> {
        use std::collections::BTreeMap;
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;

        use rustix::fs::{Mode, OFlags, open, openat};
        use sha2::{Digest as _, Sha256};

        const FILE_LIMIT: usize = 8 * 1024 * 1024;
        const TOTAL_LIMIT: usize = 32 * 1024 * 1024;
        const DIRECTORY: OFlags = OFlags::RDONLY
            .union(OFlags::DIRECTORY)
            .union(OFlags::NOFOLLOW)
            .union(OFlags::CLOEXEC);
        let Some(paths) = lake_trace_paths(targets) else {
            return Err(invalid("invalid Lake artifact trace selection"));
        };
        if !self.covers_artifact_root(&root.join(".lake")) {
            return Err(invalid("Lake artifact trace root is outside read custody"));
        }
        check_deadline()?;
        let root = open(root, DIRECTORY, Mode::empty())?;
        let mut result = BTreeMap::new();
        let mut remaining = TOTAL_LIMIT;
        for path in paths {
            check_deadline()?;
            let (directory, name) = path
                .rsplit_once('/')
                .ok_or_else(|| invalid("missing Lake trace directory"))?;
            let mut parent = None;
            for component in directory.split('/') {
                let fd = match openat(parent.as_ref().unwrap_or(&root), component, DIRECTORY, Mode::empty()) {
                    Ok(fd) => fd,
                    Err(_) => return Ok(None),
                };
                parent = Some(fd);
            }
            let fd = match openat(
                parent.as_ref().unwrap_or(&root),
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(_) => return Ok(None),
            };
            let file = File::from(fd);
            let limit = remaining.min(FILE_LIMIT);
            let Ok(metadata) = file.metadata() else {
                return Ok(None);
            };
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > limit as u64 {
                return Ok(None);
            }
            let mut bytes = Vec::new();
            let read = file.take(limit as u64 + 1).read_to_end(&mut bytes);
            check_deadline()?;
            if read.is_err() || bytes.len() > limit || !usable_lake_trace(&bytes) {
                return Ok(None);
            }
            remaining -= bytes.len();
            result.insert(path, crate::runtime_compatibility::digest_hex(Sha256::digest(&bytes)));
        }
        Ok(Some(result))
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

#[cfg(all(unix, test))]
fn lake_trace_paths(targets: &[String]) -> Option<std::collections::BTreeSet<String>> {
    if targets.is_empty() || targets.len() > 64 {
        return None;
    }
    targets
        .iter()
        .map(|target| {
            if target.is_empty()
                || target.len() > 255
                || target
                    .split('.')
                    .any(|part| part.is_empty() || !part.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '\''))
            {
                return None;
            }
            Some(if target == "runLinter" {
                ".lake/build/bin/runLinter.trace".into()
            } else {
                format!(".lake/build/lib/lean/{}.trace", target.replace('.', "/"))
            })
        })
        .collect()
}

#[cfg(all(unix, test))]
fn usable_lake_trace(bytes: &[u8]) -> bool {
    use serde_json::Value;

    fn nonempty(value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
            Value::String(value) => !value.is_empty(),
            Value::Array(value) => !value.is_empty(),
            Value::Object(value) => !value.is_empty(),
        }
    }

    let json = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    serde_json::from_slice::<Value>(json).is_ok_and(|value| {
        value.get("synthetic") == Some(&Value::Bool(false))
            && value.get("inputs").is_some_and(nonempty)
            && value.get("outputs").is_some_and(nonempty)
    })
}
