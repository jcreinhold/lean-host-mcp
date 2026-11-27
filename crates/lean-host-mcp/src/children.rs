//! The processes a server starts, and the next server's wait for them.
//!
//! Requirement: the next server serves only after every process the last one
//! started has exited. The kernel tracks this. After `server.lock`, a server
//! takes an exclusive `flock` on `<store>/children.lock` and clears its
//! close-on-exec flag, so every child it starts, and every descendant of
//! those, shares the lock's open file description. The kernel frees the lock
//! when the last of them exits. A server that finds the lock held stops the
//! process groups it finds recorded, then waits for the lock.
//!
//! Requirement: a later server signals a recorded group only while a member
//! still runs. Before a Lake build or a short run, the server creates a file
//! in `<store>/children/` and locks it; the child inherits that lock. Once the
//! child runs, the server renames the file to the child's process group and
//! closes its own copy. A held lock then means a process that inherited it
//! still runs. A free lock means none does, so the next server deletes the
//! file and never signals the group. The server that started the group checks
//! the same lock before it kills members that the group's leader left behind.
//!
//! Workers and the semantic runtime's `lake` have no file, since the SDK and
//! the search crate start them. They hold `children.lock` like every child,
//! and a worker exits once the pipe to its server closes.
//!
//! Nothing here unlocks a file. An unlock frees the lock for every process
//! that shares the open file description, children included; only the last
//! close may free it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::{Errno, FdFlags, fcntl_setfd};
use rustix::process::{Pid, Signal, kill_process_group};

/// The lock that every process a server starts inherits.
pub(crate) const LOCK_FILE_NAME: &str = "children.lock";
/// The directory of per-child files inside the store.
pub(crate) const DIR_NAME: &str = "children";
/// The name prefix of a child's file before the child runs.
const STARTING_PREFIX: &str = "starting-";
/// How often startup looks at the locks while it waits.
const POLL: Duration = Duration::from_millis(50);

/// How long startup waits for an earlier server's processes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Waits {
    /// From SIGTERM to SIGKILL for each recorded group.
    pub(crate) grace: Duration,
    /// For `children.lock`, once the recorded groups have been signalled.
    pub(crate) bound: Duration,
}

/// The waits a server uses at startup.
pub(crate) const STARTUP: Waits = Waits {
    grace: Duration::from_secs(5),
    bound: Duration::from_secs(30),
};

/// This server's hold on `children.lock`. Dropping it closes only this
/// process's copy; the lock stays held while any child still runs.
#[derive(Debug)]
pub(crate) struct ChildrenLock {
    _file: File,
}

impl ChildrenLock {
    /// Take `children.lock` in the canonical store `store`, which the caller
    /// holds through `server.lock`, waiting as `waits` says.
    ///
    /// # Errors
    /// Fails when processes of an earlier server still hold the lock after
    /// the waits, naming the groups still running, or on an I/O error.
    pub(crate) fn take_with(store: &Path, waits: Waits) -> anyhow::Result<Self> {
        let dir = store.join(DIR_NAME);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|error| anyhow::anyhow!("cannot create {}: {error}", dir.display()))?;
        let path = store.join(LOCK_FILE_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| anyhow::anyhow!("cannot open {}: {error}", path.display()))?;
        if !try_lock(&file)? {
            tracing::warn!(store = %store.display(), "processes an earlier server started still run; stopping the recorded ones");
            stop_recorded(&dir, waits.grace)?;
            if !wait_for(&file, waits.bound)? {
                return Err(still_running(store, &dir, waits));
            }
            tracing::info!(store = %store.display(), "every process of the earlier server has exited");
        }
        // No process of an earlier server runs now, so no file names one.
        clear(&dir)?;
        fcntl_setfd(&file, FdFlags::empty())
            .map_err(|error| anyhow::anyhow!("cannot pass {} to children: {error}", path.display()))?;
        Ok(Self { _file: file })
    }
}

/// Take an exclusive lock on `file` without waiting. Returns whether it was
/// free.
fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// A child's file whose lock was held when startup looked.
struct Held {
    file: File,
    path: PathBuf,
    /// The process group in the file's name. `None` for a child that was
    /// still starting when its server died.
    group: Option<Pid>,
}

/// SIGTERM each recorded group whose file lock is held, wait up to `grace`,
/// then SIGKILL each one still held. Delete each file whose lock is free; such
/// a group is never signalled.
fn stop_recorded(dir: &Path, grace: Duration) -> io::Result<()> {
    let mut held = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if try_lock(&file)? {
            remove(&path);
            continue;
        }
        let group = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|group| *group > 1)
            .and_then(Pid::from_raw);
        held.push(Held { file, path, group });
    }
    for child in &held {
        signal(child, Signal::TERM);
    }
    let started = Instant::now();
    while !held.is_empty() && started.elapsed() < grace {
        thread::sleep(POLL);
        held = still_held(held)?;
    }
    for child in still_held(held)? {
        signal(&child, Signal::KILL);
    }
    Ok(())
}

/// The children of `held` whose lock is still held. Delete the file of each
/// one that freed it.
fn still_held(held: Vec<Held>) -> io::Result<Vec<Held>> {
    let mut left = Vec::new();
    for child in held {
        if try_lock(&child.file)? {
            remove(&child.path);
        } else {
            left.push(child);
        }
    }
    Ok(left)
}

/// Signal the group of `child`, whose lock the caller has just seen held.
fn signal(child: &Held, signal: Signal) {
    let Some(group) = child.group else {
        return;
    };
    match kill_process_group(group, signal) {
        Ok(()) => tracing::info!(
            group = group.as_raw_nonzero().get(),
            signal = signal.as_raw(),
            "signalled a process group of an earlier server"
        ),
        // The group has gone. A process that left it may still hold the lock.
        Err(rustix::io::Errno::SRCH) => {}
        Err(error) => {
            let group = group.as_raw_nonzero().get();
            tracing::warn!(group, %error, "cannot signal a process group of an earlier server");
        }
    }
}

/// Wait up to `bound` for `file`'s lock. Returns whether this process got it.
fn wait_for(file: &File, bound: Duration) -> io::Result<bool> {
    let started = Instant::now();
    loop {
        if try_lock(file)? {
            return Ok(true);
        }
        if started.elapsed() >= bound {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

/// The refusal for a store whose `children.lock` stays held.
fn still_running(store: &Path, dir: &Path, waits: Waits) -> anyhow::Error {
    let mut groups = Vec::new();
    let mut starting = 0_usize;
    if let Ok(entries) = fs::read_dir(dir) {
        for path in entries.flatten().map(|entry| entry.path()) {
            let held = File::open(&path).is_ok_and(|file| matches!(try_lock(&file), Ok(false)));
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !held {
                continue;
            }
            if name.starts_with(STARTING_PREFIX) {
                starting = starting.saturating_add(1);
            } else {
                groups.push(name.to_owned());
            }
        }
    }
    groups.sort();
    let named = if groups.is_empty() {
        "no recorded process group still runs".to_owned()
    } else {
        format!("process groups {} still run", groups.join(", "))
    };
    let recorded = match starting {
        0 => named,
        1 => format!("{named}, and 1 child still runs that its server had not named"),
        _ => format!("{named}, and {starting} children still run that their server had not named"),
    };
    let lock = store.join(LOCK_FILE_NAME);
    anyhow::anyhow!(
        "processes an earlier server started still hold {} after {} s: {recorded}. Processes the server does not record, \
         such as workers whose pipe to the dead server has not closed yet, may hold it too. \
         `lsof {}` lists every holder; stop them, then start the server again",
        lock.display(),
        waits.grace.saturating_add(waits.bound).as_secs(),
        lock.display(),
    )
}

/// Delete every file in `dir`.
fn clear(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        remove(&entry?.path());
    }
    Ok(())
}

fn remove(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, path = %path.display(), "cannot delete a child's file"),
    }
}

/// A direct child's lock file: in `<store>/children/` when this process holds
/// a store, else in a directory the caller names, as in library tests. The
/// child inherits the lock. Dropping this deletes the file, so drop it only
/// once the child's group has exited, or [`keep`](Self::keep) it.
#[derive(Debug)]
pub(crate) struct ChildFile {
    /// This process's copy of the lock, until the child runs.
    file: Option<File>,
    path: PathBuf,
    keep: bool,
}

impl ChildFile {
    /// Create and lock the file for a child about to start.
    ///
    /// # Errors
    /// Returns the I/O error from creating or locking the file.
    pub(crate) fn create(elsewhere: &Path) -> io::Result<Self> {
        let dir = crate::store::process_store().map_or_else(|| elsewhere.to_path_buf(), |store| store.join(DIR_NAME));
        let path = dir.join(format!("{STARTING_PREFIX}{}", uuid::Uuid::new_v4()));
        // std opens with close-on-exec, so only the child the caller names
        // inherits this file.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let locked = try_lock(&file)?;
        let child = Self {
            file: Some(file),
            path,
            keep: false,
        };
        if !locked {
            return Err(io::Error::other(format!(
                "a new child file {} is already locked",
                child.path.display()
            )));
        }
        Ok(child)
    }

    /// The descriptor the child inherits, until [`started`](Self::started).
    pub(crate) fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.file.as_ref().map(|file| file.as_fd())
    }

    /// Record that the child runs as process group `group`: name the file by
    /// the group and close this process's copy of the lock, which the child
    /// now holds.
    pub(crate) fn started(&mut self, group: u32) {
        self.file = None;
        let named = self.path.with_file_name(group.to_string());
        match fs::rename(&self.path, &named) {
            Ok(()) => self.path = named,
            Err(error) => tracing::warn!(
                %error, path = %self.path.display(), group,
                "cannot name a child's file by its process group; a later server can only wait for the child"
            ),
        }
    }

    /// Kill process group `group`, the child's, while a member still runs:
    /// only while a process still holds this file's lock. Does nothing before
    /// [`started`](Self::started), while this process's own copy would hide
    /// the answer.
    ///
    /// # Errors
    /// Returns the I/O error from checking the lock or sending the signal.
    pub(crate) fn stop_group(&self, group: u32) -> io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let Some(pid) = i32::try_from(group)
            .ok()
            .filter(|group| *group > 1)
            .and_then(Pid::from_raw)
        else {
            return Ok(());
        };
        let probe = match File::open(&self.path) {
            Ok(probe) => probe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if try_lock(&probe)? {
            return Ok(());
        }
        match kill_process_group(pid, Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Leave the file for the next server, for a child whose exit this
    /// process cannot confirm.
    pub(crate) fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for ChildFile {
    fn drop(&mut self) {
        if !self.keep {
            remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::Command;

    use lean_toolchain::process_group::OwnedCommandGroup;
    use serial_test::serial;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const SHORT: Waits = Waits {
        grace: Duration::from_millis(200),
        bound: Duration::from_millis(300),
    };

    /// Long enough for children that other tests start, which inherit this
    /// test's lock, to exit.
    const LONG: Waits = Waits {
        grace: Duration::from_secs(5),
        bound: Duration::from_secs(10),
    };

    fn store() -> io::Result<(tempfile::TempDir, PathBuf)> {
        let tmp = tempfile::tempdir()?;
        let store = tmp.path().canonicalize()?;
        fs::create_dir(store.join(DIR_NAME))?;
        Ok((tmp, store))
    }

    fn names(dir: &Path) -> io::Result<Vec<String>> {
        let mut names = fs::read_dir(dir)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    fn refusal(store: &Path) -> String {
        ChildrenLock::take_with(store, SHORT)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    }

    #[test]
    #[serial(children_lock)]
    fn a_free_children_lock_clears_stale_files_and_passes_to_children() -> TestResult {
        let (_tmp, store) = store()?;
        let dir = store.join(DIR_NAME);
        fs::write(dir.join("4242"), b"")?;
        fs::write(dir.join("starting-x"), b"")?;
        let lock = ChildrenLock::take_with(&store, SHORT)?;
        assert!(names(&dir)?.is_empty());
        let mut child = Command::new("sleep").arg("30").spawn()?;
        drop(lock);
        // The child keeps the lock this process gave up.
        let refused = refusal(&store);
        assert!(refused.contains("lsof"), "{refused}");
        child.kill()?;
        child.wait()?;
        assert!(ChildrenLock::take_with(&store, LONG).is_ok());
        Ok(())
    }

    #[test]
    #[serial(children_lock)]
    fn a_child_file_whose_lock_is_free_is_never_signalled() -> TestResult {
        let (_tmp, store) = store()?;
        // A process with no file keeps children.lock held.
        let holder = File::create(store.join(LOCK_FILE_NAME))?;
        holder.try_lock()?;
        let mut sleeper = Command::new("sleep").arg("30").process_group(0).spawn()?;
        let file = store.join(DIR_NAME).join(sleeper.id().to_string());
        fs::write(&file, b"")?;

        let refused = refusal(&store);

        let running = sleeper.try_wait()?.is_none();
        sleeper.kill()?;
        sleeper.wait()?;
        assert!(running, "a group whose file lock is free was signalled");
        assert!(!file.exists(), "a file whose lock is free is deleted");
        assert!(refused.contains("no recorded process group still runs"), "{refused}");
        assert!(refused.contains("lsof"), "{refused}");
        drop(holder);
        Ok(())
    }

    /// The leader exits and leaves a member running, as a Lake script that
    /// starts a background process can.
    #[test]
    fn stop_group_kills_members_a_leader_left_only_while_one_holds_the_lock() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let mut file = ChildFile::create(tmp.path())?;
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let inherited: Vec<BorrowedFd<'_>> = file.as_fd().into_iter().collect();
        let mut group = OwnedCommandGroup::spawn_inheriting(command, &inherited)?;
        drop(inherited);
        file.started(group.id());
        let started = Instant::now();
        let status = loop {
            if let Some(status) = group.try_wait()? {
                break status;
            }
            if group.cleanup_pending() {
                file.stop_group(group.id())?;
            }
            if started.elapsed() > LONG.bound {
                return Err("the leftover member was not stopped".into());
            }
            thread::sleep(POLL);
        };
        assert!(status.success(), "{status:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        // Every member has exited, so the lock is free and nothing is signalled.
        file.stop_group(group.id())?;
        drop(file);
        assert!(names(tmp.path())?.is_empty(), "dropping the file deletes it");
        Ok(())
    }

    #[test]
    #[serial(children_lock)]
    fn a_held_child_file_gets_its_group_stopped_and_the_lock_frees() -> TestResult {
        let (_tmp, store) = store()?;
        let lock = File::create(store.join(LOCK_FILE_NAME))?;
        lock.try_lock()?;
        let starting = store.join(DIR_NAME).join("starting-test");
        let file = File::create(&starting)?;
        file.try_lock()?;
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut group = OwnedCommandGroup::spawn_inheriting(command, &[lock.as_fd(), file.as_fd()])?;
        let named = store.join(DIR_NAME).join(group.id().to_string());
        fs::rename(&starting, &named)?;
        // Only the sleeping child holds both locks now.
        drop((lock, file));

        let taken = ChildrenLock::take_with(&store, LONG).map(drop);

        let started = Instant::now();
        let status = loop {
            if let Some(status) = group.try_wait()? {
                break status;
            }
            if started.elapsed() > LONG.bound {
                return Err("the stopped group did not exit".into());
            }
            thread::sleep(POLL);
        };
        assert!(taken.is_ok(), "{taken:?}");
        assert!(!named.exists());
        assert_eq!(status.signal(), Some(Signal::TERM.as_raw()));
        Ok(())
    }
}
