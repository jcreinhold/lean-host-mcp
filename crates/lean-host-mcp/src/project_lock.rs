//! Fair turns between checks and builds on one project.
//!
//! Requirement: checks share the project and a build runs alone. Each
//! canonical project root has one fair read-write lock. A check takes a read
//! turn before it pins a worker or starts a short run and keeps it until the
//! request ends; a short run keeps a share of that turn until its process
//! group has exited, even past the request. A build takes the write turn.
//! Tokio grants turns in arrival order, so a queued build holds back later
//! checks, and a waiting check keeps its place in line until its deadline.
//!
//! Only this server runs Lake in the checkout (see [`crate::store`]), so these
//! turns are the only lock between checks and builds.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio::time::Instant;

use crate::cancellation::CallCancellation;

/// A share of a request's read turn. The turn passes to a build only when
/// every share has dropped.
pub(crate) type ReadShare = Arc<OwnedRwLockReadGuard<()>>;

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
    /// The request already holds a turn on this other root.
    OtherRoot(PathBuf),
    /// The request holds a read turn, so its write turn would wait for itself.
    HoldsRead,
}

/// A request's read turn.
struct HeldRead {
    turn: ReadShare,
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
}

/// Run a request with a turn slot. A scope made inside another shares the
/// outer slot. This is a plain function, not an `async fn`, so the future
/// holds `operation` once rather than twice.
pub(crate) fn scope<F: Future>(operation: F) -> impl Future<Output = F::Output> {
    let slot = TURN.try_with(Arc::clone).unwrap_or_default();
    TURN.scope(slot, operation)
}

/// Give the current request a read turn on `root` and hold it until the
/// request ends. Returns the finished-build count at the grant. A request reuses its turn: a second turn queued behind a
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
    *held = Some(HeldRead {
        turn: Arc::new(turn),
        root: root.to_path_buf(),
        granted_at,
    });
    Ok(granted_at)
}

/// A share of the current request's read turn on `root`, for a short run to
/// keep until its process group has exited. `None` outside a request scope or
/// before the request holds a turn there.
pub(crate) async fn read_share(root: &Path) -> Option<ReadShare> {
    let slot = TURN.try_with(Arc::clone).ok()?;
    let held = slot.lock().await;
    held.as_ref()
        .filter(|read| read.root == root)
        .map(|read| Arc::clone(&read.turn))
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
