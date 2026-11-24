//! Keep actor capacity until every tracked child has a confirmed exit.
//!
//! The scope guard must outlive all code that can spawn a child for its actor.
//! Its destruction seals that scope, including during unwinding. A zero count
//! before then is insufficient: the actor may still start another generation.
//! This registry survives failed cleanup-thread creation and is also polled by
//! broker observation/admission. It covers direct tracked children in this
//! process, not server crashes or independently spawned Lake descendants.

use std::io;
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::Duration;

use lean_rs_worker_parent::LeanWorkerProcessTracker;
use parking_lot::Mutex;
use tokio::sync::OwnedSemaphorePermit;

const RETIREMENT_POLL_INTERVAL: Duration = Duration::from_millis(100);

static RETIREMENTS: LazyLock<Arc<RetirementRegistry>> = LazyLock::new(|| Arc::new(RetirementRegistry::default()));

/// Actor-scope ownership of a slot and all ordinary/companion child generations.
pub(crate) struct WorkerSlot {
    retirement: Option<RetiringWorker>,
    registry: Arc<RetirementRegistry>,
}

impl WorkerSlot {
    pub(crate) fn new(permit: OwnedSemaphorePermit, tracker: LeanWorkerProcessTracker) -> Self {
        Self {
            retirement: Some(RetiringWorker {
                _permit: permit,
                scope: RetiringScope::Worker(tracker),
            }),
            registry: Arc::clone(&RETIREMENTS),
        }
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        if let Some(retirement) = self.retirement.take() {
            self.registry.state.lock().pending.push(retirement);
            self.registry.poll();
        }
    }
}

/// Retry late exit observation, including after cleanup-thread creation fails.
pub(crate) fn poll_retirements() {
    RETIREMENTS.poll();
}

struct RetiringWorker {
    _permit: OwnedSemaphorePermit,
    scope: RetiringScope,
}

enum RetiringScope {
    Worker(LeanWorkerProcessTracker),
    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    Writer {
        observation: lean_toolchain::shared_writer::WriterExitObservation,
        dispatch_attempted: bool,
    },
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
pub(crate) struct ManagedWriterSlot {
    permit: Option<OwnedSemaphorePermit>,
    observation: Option<lean_toolchain::shared_writer::WriterExitObservation>,
    dispatch_attempted: bool,
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
impl ManagedWriterSlot {
    pub(crate) fn new(permit: OwnedSemaphorePermit) -> Self {
        Self {
            permit: Some(permit),
            observation: None,
            dispatch_attempted: false,
        }
    }

    pub(crate) fn observation(&mut self) -> &mut Option<lean_toolchain::shared_writer::WriterExitObservation> {
        &mut self.observation
    }

    pub(crate) fn begin_dispatch(&mut self) {
        self.dispatch_attempted = true;
    }
}

#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
impl Drop for ManagedWriterSlot {
    fn drop(&mut self) {
        // The SDK sets the observer at spawn, including on a failed Ready.
        // Dispatch uncertainty and exact exit evidence move together into retirement.
        let retirement = (self.permit.take(), self.observation.take());
        if let (Some(permit), Some(observation)) = retirement {
            RETIREMENTS.state.lock().pending.push(RetiringWorker {
                _permit: permit,
                scope: RetiringScope::Writer {
                    observation,
                    dispatch_attempted: self.dispatch_attempted,
                },
            });
            RETIREMENTS.poll();
        }
    }
}

#[derive(Default)]
struct RetirementState {
    pending: Vec<RetiringWorker>,
    running: bool,
}

impl RetirementState {
    fn reap(&mut self) {
        // This also drives the upstream tracker's pending-Child cleanup when
        // its own best-effort reaper thread could not be created. Only a
        // confirmed wait/try_wait result decreases the tracker count.
        self.pending.retain(|worker| match &worker.scope {
            RetiringScope::Worker(tracker) => tracker.active_processes() != 0,
            #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
            RetiringScope::Writer {
                observation,
                dispatch_attempted,
            } => !observation.status().is_some_and(|status| {
                status.success() || (!dispatch_attempted && writer_locks_released(observation.lock_identities()))
            }),
        });
    }
}

// Reacquire the complete original union together. No replacement inode or
// partially acquired union establishes closure. Contention is retried by the
// existing retirement poller.
#[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
fn writer_locks_released(identities: &[lean_toolchain::shared_writer::WriterLockIdentity]) -> bool {
    use std::fs::{File, symlink_metadata};
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{Mode, OFlags, open};

    if identities.is_empty() {
        return false;
    }
    let mut custody = Vec::with_capacity(identities.len());
    for identity in identities {
        let Ok(fd) = open(
            &identity.path,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return false;
        };
        let file = File::from(fd);
        if !file.metadata().is_ok_and(|metadata| {
            metadata.is_file() && (metadata.dev(), metadata.ino()) == (identity.device, identity.inode)
        }) || file.try_lock().is_err()
        {
            return false;
        }
        custody.push(file);
    }
    let same_paths = identities.iter().all(|identity| {
        symlink_metadata(&identity.path).is_ok_and(|metadata| {
            metadata.is_file() && (metadata.dev(), metadata.ino()) == (identity.device, identity.inode)
        })
    });
    drop(custody);
    same_paths
}

#[derive(Default)]
struct RetirementRegistry {
    state: Mutex<RetirementState>,
}

impl RetirementRegistry {
    fn poll(self: &Arc<Self>) {
        self.poll_with(|registry| {
            thread::Builder::new()
                .name("lean-host-worker-retirement".into())
                .spawn(move || registry.run())
                .map(|_thread| ())
        });
    }

    fn poll_with(self: &Arc<Self>, spawn: impl FnOnce(Arc<Self>) -> io::Result<()>) {
        {
            let mut state = self.state.lock();
            state.reap();
            if state.pending.is_empty() || state.running {
                return;
            }
            state.running = true;
        }
        if let Err(error) = spawn(Arc::clone(self)) {
            // No entry moved into the closure. The process-global registry
            // still owns every tracker and permit; the next broker poll
            // retries both confirmed-exit observation and thread creation.
            self.state.lock().running = false;
            tracing::warn!(%error, "worker retirement thread unavailable; retaining capacity until a later poll");
        }
    }

    fn run(&self) {
        loop {
            {
                let mut state = self.state.lock();
                state.reap();
                if state.pending.is_empty() {
                    // Enqueue and this transition share the same lock, so an
                    // arriving retirement either has a poller or starts one.
                    state.running = false;
                    drop(state);
                    return;
                }
            }
            thread::sleep(RETIREMENT_POLL_INTERVAL);
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Instant;

    use lean_rs_worker_parent::{LeanWorker, LeanWorkerConfig};
    use lean_rs_worker_protocol::protocol::{MAX_FRAME_BYTES, Message, PROTOCOL_VERSION, write_frame};
    use tokio::sync::Semaphore;

    use super::*;

    fn fake_worker_script(root: &Path) -> std::path::PathBuf {
        let mut handshake = Vec::new();
        write_frame(
            &mut handshake,
            Message::Handshake {
                worker_version: "host-retirement-test".into(),
                protocol_version: PROTOCOL_VERSION,
            },
            MAX_FRAME_BYTES,
        )
        .expect("encode fake handshake");
        fs::write(root.join("handshake"), handshake).expect("write fake handshake");
        let executable = root.join("fake-worker");
        fs::write(&executable, "#!/bin/sh\n/bin/cat handshake\nexec /bin/sleep 60\n").expect("write fake worker");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("make fake worker executable");
        executable
    }

    fn worker(root: &Path, tracker: &LeanWorkerProcessTracker) -> LeanWorker {
        LeanWorker::spawn(
            &LeanWorkerConfig::new(fake_worker_script(root))
                .current_dir(root)
                .process_tracker(tracker.clone())
                .startup_timeout(Duration::from_secs(5))
                .shutdown_timeout(Duration::from_millis(20)),
        )
        .expect("spawn tracked fake child")
    }

    fn slot(semaphore: &Arc<Semaphore>, tracker: LeanWorkerProcessTracker) -> WorkerSlot {
        WorkerSlot::new(
            Arc::clone(semaphore).try_acquire_owned().expect("reserve actor slot"),
            tracker,
        )
    }

    fn wait_for_capacity(semaphore: &Semaphore) {
        let started = Instant::now();
        while semaphore.available_permits() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "confirmed retirement did not release capacity"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn zero_count_cannot_release_capacity_before_actor_scope_ends() {
        let semaphore = Arc::new(Semaphore::new(1));
        let tracker = LeanWorkerProcessTracker::new();
        let scope = slot(&semaphore, tracker.clone());
        assert_eq!(tracker.active_processes(), 0);
        poll_retirements();
        assert_eq!(semaphore.available_permits(), 0);

        let root = tempfile::tempdir().expect("fake child directory");
        let child = worker(root.path(), &tracker);
        assert_eq!(tracker.active_processes(), 1);
        child.shutdown().expect("stop tracked child");
        assert_eq!(tracker.active_processes(), 0);
        assert_eq!(semaphore.available_permits(), 0);
        drop(scope);
        wait_for_capacity(&semaphore);
    }

    #[test]
    fn late_retirement_waits_for_ordinary_and_companion_child_and_releases_once() {
        let semaphore = Arc::new(Semaphore::new(1));
        let tracker = LeanWorkerProcessTracker::new();
        let scope = slot(&semaphore, tracker.clone());
        let root = tempfile::tempdir().expect("fake children directory");
        let ordinary = worker(root.path(), &tracker);
        let companion = worker(root.path(), &tracker);
        assert_eq!(tracker.active_processes(), 2);

        // No code below can spawn another generation. Keep the existing
        // children alive to make delayed exit accounting observable.
        drop(scope);
        poll_retirements();
        assert_eq!(semaphore.available_permits(), 0);
        ordinary.shutdown().expect("stop ordinary child");
        assert_eq!(tracker.active_processes(), 1);
        poll_retirements();
        assert_eq!(semaphore.available_permits(), 0);
        companion.shutdown().expect("stop companion child");
        wait_for_capacity(&semaphore);
        for _ in 0..3 {
            poll_retirements();
            assert_eq!(semaphore.available_permits(), 1);
        }
    }

    #[test]
    fn actor_unwind_drops_owned_child_before_releasing_its_slot() {
        let semaphore = Arc::new(Semaphore::new(1));
        let tracker = LeanWorkerProcessTracker::new();
        let root = tempfile::tempdir().expect("fake child directory");
        let scope = slot(&semaphore, tracker.clone());
        let (started, observed) = mpsc::sync_channel(0);
        let (proceed, resumed) = mpsc::sync_channel(0);
        let observed_tracker = tracker.clone();
        let actor = thread::spawn(move || {
            // Declared before the child, as in the project actor integration.
            let _scope = scope;
            let _child = worker(root.path(), &tracker);
            started.send(()).expect("observe child before actor panic");
            resumed
                .recv_timeout(Duration::from_secs(5))
                .expect("resume actor after observation");
            panic!("injected actor startup panic");
        });
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("actor started its child");
        assert_eq!(semaphore.available_permits(), 0);
        proceed.send(()).expect("allow injected panic");
        assert!(actor.join().is_err());
        wait_for_capacity(&semaphore);
        assert_eq!(observed_tracker.active_processes(), 0);
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    fn failed_ready_observation(root: &Path) -> lean_toolchain::shared_writer::WriterExitObservation {
        use lean_toolchain::shared_writer::SharedWriter;

        crate::shared_writer::ownership_registry().expect("test host ownership registry");
        let project = root.join("demo");
        fs::create_dir(&project).expect("create disposable project");
        fs::write(project.join("lakefile.toml"), "name = 'demo'\n").expect("write Lake project");
        fs::write(
            project.join("lake-manifest.json"),
            br#"{"version":"1.1.0","packagesDir":".lake/packages","packages":[],"name":"demo","lakeDir":".lake"}"#,
        )
        .expect("write manifest");
        let helper = root.join("bad-ready");
        fs::write(&helper, "#!/bin/sh\nprintf '{}\\n'\nexit 23\n").expect("write invalid-Ready helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("make helper executable");
        let mut observed = None;
        let error = SharedWriter::open_with_write_roots_handoff_observed(
            &helper,
            &project,
            &[],
            Duration::from_secs(5),
            &|| false,
            &mut observed,
        )
        .err()
        .expect("refuse invalid Ready");
        assert!(error.to_string().contains("ownership/lock-custody handshake failed"));
        let observed = observed.expect("exact failed helper spawned");
        let started = Instant::now();
        loop {
            if let Some(status) = observed.status() {
                assert_eq!(status.code(), Some(23));
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "helper exit not observed");
            std::thread::sleep(Duration::from_millis(5));
        }
        observed
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    fn wait_for_writer_locks(mut settled: impl FnMut() -> bool) {
        // A concurrent fork may retain a copy of a loaned file description
        // until exec. Exact helper exit alone does not prove lock closure.
        let started = Instant::now();
        while !settled() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "original writer locks did not settle"
            );
            thread::sleep(RETIREMENT_POLL_INTERVAL);
        }
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn failed_ready_returns_capacity_after_exact_lock_contention_clears() {
        let root = tempfile::tempdir().expect("disposable writer directory");
        let observation = failed_ready_observation(&root.path().canonicalize().expect("canonical fixture"));
        let identity = observation.lock_identities().first().expect("original lock identity");
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&identity.path)
            .expect("open exact original lock");
        wait_for_writer_locks(|| match contender.try_lock() {
            Ok(()) => true,
            Err(fs::TryLockError::WouldBlock) => false,
            Err(error) => panic!("could not lock original inode: {error}"),
        });
        let slots = Arc::new(Semaphore::new(1));
        let mut state = RetirementState::default();
        state.pending.push(RetiringWorker {
            _permit: Arc::clone(&slots).try_acquire_owned().expect("reserve writer slot"),
            scope: RetiringScope::Writer {
                observation,
                dispatch_attempted: false,
            },
        });
        state.reap();
        assert_eq!(slots.available_permits(), 0);
        contender.unlock().expect("release the fixture's original lock");
        wait_for_writer_locks(|| {
            state.reap();
            state.pending.is_empty()
        });
        assert_eq!(slots.available_permits(), 1);
        assert!(state.pending.is_empty());
        state.reap();
        assert_eq!(slots.available_permits(), 1);
    }

    #[cfg(all(feature = "managed-native-store", any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn dispatch_uncertainty_keeps_capacity_after_exact_failed_exit_and_free_locks() {
        let root = tempfile::tempdir().expect("disposable writer directory");
        let observation = failed_ready_observation(&root.path().canonicalize().expect("canonical fixture"));
        wait_for_writer_locks(|| writer_locks_released(observation.lock_identities()));
        let slots = Arc::new(Semaphore::new(1));
        let mut state = RetirementState::default();
        state.pending.push(RetiringWorker {
            _permit: Arc::clone(&slots).try_acquire_owned().expect("reserve writer slot"),
            // Exercise the same boundary as a failed or partial dispatch write.
            scope: RetiringScope::Writer {
                observation,
                dispatch_attempted: true,
            },
        });
        state.reap();
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn cleanup_thread_creation_failure_keeps_tracker_and_permit_for_later_poll() {
        let semaphore = Arc::new(Semaphore::new(1));
        let tracker = LeanWorkerProcessTracker::new();
        let root = tempfile::tempdir().expect("fake child directory");
        let child = worker(root.path(), &tracker);
        let registry = Arc::new(RetirementRegistry::default());
        registry.state.lock().pending.push(RetiringWorker {
            _permit: Arc::clone(&semaphore).try_acquire_owned().expect("reserve actor slot"),
            scope: RetiringScope::Worker(tracker.clone()),
        });
        registry.poll_with(|_registry| Err(io::Error::other("injected thread creation failure")));
        assert_eq!(registry.state.lock().pending.len(), 1);
        assert!(!registry.state.lock().running);
        assert_eq!(semaphore.available_permits(), 0);
        assert_eq!(tracker.active_processes(), 1);

        child.shutdown().expect("stop retained child");
        registry.poll_with(|_registry| panic!("an empty retirement queue must not create a thread"));
        assert_eq!(semaphore.available_permits(), 1);
        assert!(registry.state.lock().pending.is_empty());
        drop(registry);
    }
}
