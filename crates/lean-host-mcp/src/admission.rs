//! Cross-process admission control for Lean semantic work.
//!
//! `SemanticAdmission` is the one boundary that knows how the global limit is
//! enforced. Callers ask for one opaque permit; the implementation combines a
//! per-process waiter bound with a per-user advisory-lock pool on disk.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs4::{FileExt, TryLockError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Async admission for heavy Lean semantic work.
#[derive(Debug)]
pub(crate) struct SemanticAdmission {
    permits: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
    wait_timeout: Duration,
    global: Arc<GlobalAdmission>,
}

impl SemanticAdmission {
    pub(crate) fn new(
        permits: NonZeroUsize,
        waiter_capacity: NonZeroUsize,
        wait_timeout: Duration,
        lock_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(permits.get())),
            waiters: Arc::new(Semaphore::new(waiter_capacity.get())),
            wait_timeout,
            global: Arc::new(GlobalAdmission {
                permits: permits.get(),
                lock_dir,
            }),
        })
    }

    pub(crate) async fn acquire(self: &Arc<Self>) -> std::result::Result<SemanticPermit, AdmissionError> {
        let start = Instant::now();
        let waiter = Arc::clone(&self.waiters).try_acquire_owned().map_err(|err| match err {
            tokio::sync::TryAcquireError::NoPermits => AdmissionError::Full,
            tokio::sync::TryAcquireError::Closed => AdmissionError::Closed,
        })?;

        let local = tokio::time::timeout(self.wait_timeout, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| AdmissionError::Timeout)?
            .map_err(|_| AdmissionError::Closed)?;

        let remaining = self
            .wait_timeout
            .checked_sub(start.elapsed())
            .ok_or(AdmissionError::Timeout)?;
        let global = tokio::time::timeout(remaining, self.global.acquire()).await;
        let global = match global {
            Ok(Ok(permit)) => permit,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(AdmissionError::Timeout),
        };
        drop(waiter);
        Ok(SemanticPermit {
            _local: local,
            _global: global,
        })
    }
}

/// Opaque owned semantic permit. Dropping it releases both local and global
/// admission slots.
#[derive(Debug)]
pub(crate) struct SemanticPermit {
    _local: OwnedSemaphorePermit,
    _global: GlobalPermit,
}

#[derive(Debug)]
struct GlobalAdmission {
    permits: usize,
    lock_dir: PathBuf,
}

impl GlobalAdmission {
    async fn acquire(&self) -> std::result::Result<GlobalPermit, AdmissionError> {
        loop {
            let lock_dir = self.lock_dir.clone();
            let permits = self.permits;
            let attempt = tokio::task::spawn_blocking(move || try_acquire_global(&lock_dir, permits))
                .await
                .map_err(|err| AdmissionError::Config(format!("global admission task failed: {err}")))?;
            match attempt? {
                Some(permit) => return Ok(permit),
                None => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
}

#[derive(Debug)]
struct GlobalPermit {
    file: File,
}

impl Drop for GlobalPermit {
    fn drop(&mut self) {
        drop(FileExt::unlock(&self.file));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AdmissionError {
    Full,
    Timeout,
    Closed,
    Config(String),
}

impl AdmissionError {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Full => "semantic_admission_full",
            Self::Timeout => "semantic_admission_timeout",
            Self::Closed => "semantic_admission_closed",
            Self::Config(_) => "semantic_admission_config",
        }
    }

    pub(crate) const fn retryable(&self) -> bool {
        !matches!(self, Self::Closed | Self::Config(_))
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            Self::Config(detail) => Some(detail.as_str()),
            Self::Full | Self::Timeout | Self::Closed => None,
        }
    }
}

pub(crate) fn default_lock_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lean-host-mcp")
        .join("semantic-admission")
}

fn try_acquire_global(lock_dir: &Path, permits: usize) -> std::result::Result<Option<GlobalPermit>, AdmissionError> {
    fs::create_dir_all(lock_dir).map_err(|err| {
        AdmissionError::Config(format!(
            "create semantic admission lock dir {}: {err}",
            lock_dir.display()
        ))
    })?;
    reconcile_limit(lock_dir, permits)?;
    for slot in 0..permits {
        let path = lock_dir.join(format!("permit-{slot:03}.lock"));
        let mut file = open_lock_file(&path)?;
        match FileExt::try_lock(&file) {
            Ok(()) => {
                write_holder(&mut file, permits, slot);
                return Ok(Some(GlobalPermit { file }));
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => {
                return Err(AdmissionError::Config(format!(
                    "lock semantic permit {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(None)
}

fn reconcile_limit(lock_dir: &Path, permits: usize) -> std::result::Result<(), AdmissionError> {
    let limit_path = lock_dir.join("limit");
    let pool_path = lock_dir.join("pool.lock");
    let pool = open_lock_file(&pool_path)?;
    FileExt::lock(&pool)
        .map_err(|err| AdmissionError::Config(format!("lock semantic pool {}: {err}", pool_path.display())))?;
    let result = reconcile_limit_locked(lock_dir, &limit_path, permits);
    drop(FileExt::unlock(&pool));
    result
}

fn reconcile_limit_locked(
    lock_dir: &Path,
    limit_path: &Path,
    permits: usize,
) -> std::result::Result<(), AdmissionError> {
    let old = read_limit(limit_path)?;
    if old == Some(permits) {
        return Ok(());
    }
    if let Some(old_permits) = old
        && active_permit_exists(lock_dir, old_permits.max(permits))?
    {
        return Err(AdmissionError::Config(format!(
            "semantic admission lock dir {} is configured for {old_permits} permits but this server requested {permits}; stop existing servers or set LEAN_HOST_MCP_SEMANTIC_LOCK_DIR to a fresh directory",
            lock_dir.display()
        )));
    }
    fs::write(limit_path, format!("{permits}\n")).map_err(|err| {
        AdmissionError::Config(format!(
            "write semantic admission limit {}: {err}",
            limit_path.display()
        ))
    })
}

fn active_permit_exists(lock_dir: &Path, slots: usize) -> std::result::Result<bool, AdmissionError> {
    for slot in 0..slots {
        let path = lock_dir.join(format!("permit-{slot:03}.lock"));
        if !path.exists() {
            continue;
        }
        let file = open_lock_file(&path)?;
        match FileExt::try_lock(&file) {
            Ok(()) => {
                drop(FileExt::unlock(&file));
            }
            Err(TryLockError::WouldBlock) => return Ok(true),
            Err(TryLockError::Error(err)) => {
                return Err(AdmissionError::Config(format!(
                    "inspect semantic permit {}: {err}",
                    path.display()
                )));
            }
        }
    }
    Ok(false)
}

fn open_lock_file(path: &Path) -> std::result::Result<File, AdmissionError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|err| AdmissionError::Config(format!("open semantic admission lock {}: {err}", path.display())))
}

fn read_limit(path: &Path) -> std::result::Result<Option<usize>, AdmissionError> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(AdmissionError::Config(format!(
                "read semantic admission limit {}: {err}",
                path.display()
            )));
        }
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| AdmissionError::Config(format!("read semantic admission limit {}: {err}", path.display())))?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed.parse::<usize>().map(Some).map_err(|err| {
        AdmissionError::Config(format!(
            "semantic admission limit {} is not a usize: {err}",
            path.display()
        ))
    })
}

fn write_holder(file: &mut File, permits: usize, slot: usize) {
    let pid = std::process::id();
    let acquired_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    drop(file.set_len(0));
    drop(file.seek(SeekFrom::Start(0)));
    drop(writeln!(
        file,
        "pid={pid}\nslot={slot}\npermits={permits}\nacquired_unix_secs={acquired_unix_secs}"
    ));
    drop(file.sync_data());
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn nz(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[tokio::test]
    async fn global_admission_is_shared_between_instances() {
        let tmp = tempfile::tempdir().unwrap();
        let a = SemanticAdmission::new(nz(1), nz(1), Duration::from_secs(5), tmp.path().to_path_buf());
        let b = SemanticAdmission::new(nz(1), nz(1), Duration::from_millis(20), tmp.path().to_path_buf());

        let held = a.acquire().await.expect("first permit");
        assert_eq!(b.acquire().await.unwrap_err(), AdmissionError::Timeout);
        drop(held);
        let c = SemanticAdmission::new(nz(1), nz(1), Duration::from_secs(5), tmp.path().to_path_buf());
        drop(c.acquire().await.expect("released permit"));
    }

    #[tokio::test]
    async fn local_admission_bounds_waiters() {
        let tmp = tempfile::tempdir().unwrap();
        let admission = SemanticAdmission::new(nz(1), nz(1), Duration::from_secs(5), tmp.path().to_path_buf());
        let held = admission.acquire().await.expect("initial permit");
        let waiting_admission = Arc::clone(&admission);
        let waiting = tokio::spawn(async move { waiting_admission.acquire().await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(admission.acquire().await.unwrap_err(), AdmissionError::Full);

        drop(held);
        drop(waiting.await.expect("waiter task").expect("waiter permit"));
    }

    #[tokio::test]
    async fn mismatched_limit_is_rejected_while_active() {
        let tmp = tempfile::tempdir().unwrap();
        let one = SemanticAdmission::new(nz(1), nz(1), Duration::from_secs(5), tmp.path().to_path_buf());
        let held = one.acquire().await.expect("initial permit");
        let two = SemanticAdmission::new(nz(2), nz(1), Duration::from_secs(5), tmp.path().to_path_buf());

        assert!(matches!(two.acquire().await.unwrap_err(), AdmissionError::Config(_)));
        drop(held);
    }
}
