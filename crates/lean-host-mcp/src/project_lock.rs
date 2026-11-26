//! Fair turns between checks and writers on one project.
//!
//! Each canonical project root has one fair read-write lock. A check takes a
//! read turn before it pins a worker or starts a native reader, then the
//! shared kernel guard G, and keeps both until the request ends. A writer
//! takes the write turn. Tokio grants turns in arrival order, so a queued
//! writer holds back later checks, and a waiting check keeps its place in line
//! until its deadline.
//!
//! The reaper queues a proxy write turn for a writer in another process while
//! that writer's demand lasts, so new checks queue behind it here as well.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::artifact_reads::ArtifactReadGuard;
use crate::cancellation::CallCancellation;

/// The writer a proxy turn names to the checks that wait behind it.
pub(crate) const EXTERNAL_WRITER: &str = "external_writer";

tokio::task_local! {
    static TURN: Arc<tokio::sync::Mutex<Option<HeldRead>>>;
}

/// One lock per canonical project root, made on first use.
#[derive(Default)]
pub(crate) struct ProjectLocks {
    roots: HashMap<PathBuf, Arc<ProjectLock>>,
}

impl ProjectLocks {
    pub(crate) fn get(&mut self, root: &Path) -> Arc<ProjectLock> {
        Arc::clone(self.roots.entry(root.to_path_buf()).or_default())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Arc<ProjectLock>)> {
        self.roots.iter()
    }
}

/// Turns on one project root.
#[derive(Default)]
pub(crate) struct ProjectLock {
    rw: Arc<RwLock<()>>,
    /// Write turns that ended with `finish`. A reader granted after a finish
    /// sees the higher count.
    builds_finished: AtomicU64,
    /// Writers that hold or wait for the write turn, in arrival order.
    writers: Mutex<Writers>,
    /// Ends the current proxy turn.
    proxy: Mutex<Option<CancellationToken>>,
}

#[derive(Default)]
struct Writers {
    next: u64,
    line: VecDeque<(u64, String)>,
}

/// Why a request got no turn.
#[derive(Debug)]
pub(crate) enum TurnRefused {
    /// The deadline passed while the request waited. Names the writer ahead
    /// of it when one is known.
    Expired {
        blocked_by: Option<String>,
    },
    Cancelled,
    /// The kernel guard failed for a reason other than time.
    Guard(io::Error),
    /// The request already holds a turn on this other root.
    OtherRoot(PathBuf),
    /// The request holds a read turn, so its write turn would wait for itself.
    HoldsRead,
}

/// What the reaper saw of writer demand on a root.
#[derive(Clone, Copy)]
pub(crate) enum Demand {
    Present,
    Absent,
    /// Another reader held its brief gate, so nothing was learned.
    Unknown,
}

/// A request's read turn and guard G. Fields drop in order, so G closes
/// before the turn passes to a writer.
struct HeldRead {
    _guard: ArtifactReadGuard,
    _turn: OwnedRwLockReadGuard<()>,
    root: PathBuf,
    granted_at: u64,
}

/// Takes its writer out of line on drop, granted or not.
struct WriterTicket {
    lock: Arc<ProjectLock>,
    id: u64,
}

impl Drop for WriterTicket {
    fn drop(&mut self) {
        self.lock.writers.lock().line.retain(|(id, _)| *id != self.id);
    }
}

/// A granted write turn. Dropping it without `finish` releases the turn
/// without counting a build.
pub(crate) struct WriteTurn {
    ticket: WriterTicket,
    _guard: OwnedRwLockWriteGuard<()>,
}

impl WriteTurn {
    /// Count the turn as a finished build, then release it, so the next
    /// reader sees the new count.
    pub(crate) fn finish(self) {
        self.ticket.lock.builds_finished.fetch_add(1, Ordering::AcqRel);
    }
}

impl ProjectLock {
    /// A read turn only when one is free now and no writer waits. Idle work
    /// takes this and skips its work otherwise.
    pub(crate) fn try_read(&self) -> Option<OwnedRwLockReadGuard<()>> {
        Arc::clone(&self.rw).try_read_owned().ok()
    }

    /// Write turns that ended with `finish`. Stable while any read turn on
    /// this root is held.
    pub(crate) fn builds_finished(&self) -> u64 {
        self.builds_finished.load(Ordering::Acquire)
    }

    /// The writer at the front of the line, which every new reader waits for.
    fn blocked_by(&self) -> Option<String> {
        self.writers.lock().line.front().map(|(_, name)| name.clone())
    }

    /// The first writer in line ahead of writer `id`.
    fn writer_ahead_of(&self, id: u64) -> Option<String> {
        let writers = self.writers.lock();
        writers
            .line
            .iter()
            .take_while(|(other, _)| *other != id)
            .map(|(_, name)| name.clone())
            .next()
    }

    fn enter_writer(self: &Arc<Self>, name: &str) -> WriterTicket {
        let mut writers = self.writers.lock();
        let id = writers.next;
        writers.next = id.wrapping_add(1);
        writers.line.push_back((id, name.to_owned()));
        drop(writers);
        WriterTicket {
            lock: Arc::clone(self),
            id,
        }
    }

    /// Hold a proxy write turn while another process's writer demand lasts.
    /// Skips a root whose write turn this server holds or waits for. Once
    /// queued, a proxy turn always ends with `finish`, even when the demand
    /// ends before the grant, since that writer may have changed artifacts.
    pub(crate) fn observe_demand(self: &Arc<Self>, demand: Demand, handle: &tokio::runtime::Handle) {
        let mut proxy = self.proxy.lock();
        match demand {
            Demand::Unknown => {}
            Demand::Absent => {
                if let Some(ended) = proxy.take() {
                    ended.cancel();
                }
            }
            Demand::Present => {
                if proxy.is_some() || !self.writers.lock().line.is_empty() {
                    return;
                }
                let ticket = self.enter_writer(EXTERNAL_WRITER);
                // Poll once here so the turn takes its place in line now, not
                // when the task first runs.
                let mut grant = Box::pin(Arc::clone(&self.rw).write_owned());
                let first = grant.as_mut().poll(&mut Context::from_waker(Waker::noop()));
                let ended = CancellationToken::new();
                let watch = ended.clone();
                handle.spawn(async move {
                    let guard = match first {
                        Poll::Ready(guard) => guard,
                        Poll::Pending => grant.await,
                    };
                    watch.cancelled().await;
                    WriteTurn { ticket, _guard: guard }.finish();
                });
                *proxy = Some(ended);
            }
        }
    }
}

/// Run a request with a turn slot. A scope made inside another shares the
/// outer slot. This is a plain function, not an `async fn`, so the future
/// holds `operation` once rather than twice.
pub(crate) fn scope<F: Future>(operation: F) -> impl Future<Output = F::Output> {
    let slot = TURN.try_with(Arc::clone).unwrap_or_default();
    TURN.scope(slot, operation)
}

/// Give the current request a read turn on `root`, then the kernel guard G,
/// and hold both until the request ends. Returns the finished-build count at
/// the grant. A request reuses its turn: a second turn queued behind a
/// waiting writer would never be granted. Outside a request scope this takes
/// nothing and returns the current count.
pub(crate) async fn ensure_read(
    lock: &ProjectLock,
    root: &Path,
    deadline: Instant,
    cancellation: &CallCancellation,
) -> Result<u64, TurnRefused> {
    let Ok(slot) = TURN.try_with(Arc::clone) else {
        return Ok(lock.builds_finished());
    };
    let mut held = slot.lock().await;
    if let Some(read) = held.as_ref() {
        return if read.root == root {
            Ok(read.granted_at)
        } else {
            Err(TurnRefused::OtherRoot(read.root.clone()))
        };
    }
    let turn = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(TurnRefused::Cancelled),
        turn = tokio::time::timeout_at(deadline, Arc::clone(&lock.rw).read_owned()) => turn
            .map_err(|_| TurnRefused::Expired { blocked_by: lock.blocked_by() })?,
    };
    let granted_at = lock.builds_finished();
    let guard_root = root.to_path_buf();
    let remaining = deadline.saturating_duration_since(Instant::now());
    let guard = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(TurnRefused::Cancelled),
        guard = tokio::task::spawn_blocking(move || ArtifactReadGuard::acquire(&guard_root, remaining)) => guard,
    };
    let guard = match guard {
        Ok(Ok(guard)) => guard,
        // Only a writer outside this server's turns holds the kernel lock now.
        Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => {
            return Err(TurnRefused::Expired {
                blocked_by: Some(EXTERNAL_WRITER.to_owned()),
            });
        }
        Ok(Err(error)) => return Err(TurnRefused::Guard(error)),
        Err(error) => return Err(TurnRefused::Guard(io::Error::other(error))),
    };
    *held = Some(HeldRead {
        _guard: guard,
        _turn: turn,
        root: root.to_path_buf(),
        granted_at,
    });
    Ok(granted_at)
}

/// Give the current request the write turn on `lock`'s root, naming it `name`
/// to the requests that wait behind it. The writer takes its place in line
/// before it waits, so readers that come later name it as `blocked_by`.
pub(crate) async fn ensure_write(
    lock: &Arc<ProjectLock>,
    name: &str,
    deadline: Instant,
    cancellation: &CallCancellation,
) -> Result<WriteTurn, TurnRefused> {
    if let Ok(slot) = TURN.try_with(Arc::clone)
        && slot.lock().await.is_some()
    {
        return Err(TurnRefused::HoldsRead);
    }
    let ticket = lock.enter_writer(name);
    let guard = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(TurnRefused::Cancelled),
        guard = tokio::time::timeout_at(deadline, Arc::clone(&lock.rw).write_owned()) => guard
            .map_err(|_| TurnRefused::Expired { blocked_by: lock.writer_ahead_of(ticket.id) })?,
    };
    Ok(WriteTurn { ticket, _guard: guard })
}

/// The finished-build count at the current request's read turn on `root`.
/// `None` outside a request scope, before the request holds a turn there, or
/// while another part of the request waits for one.
pub(crate) fn granted_at(root: &Path) -> Option<u64> {
    TURN.try_with(|slot| {
        let held = slot.try_lock().ok()?;
        held.as_ref()
            .filter(|read| read.root == root)
            .map(|read| read.granted_at)
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests;
