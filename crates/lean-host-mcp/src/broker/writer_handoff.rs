//! Hand artifacts to a writer in another process.
//!
//! A writer announces demand before it takes the kernel lock exclusively. The
//! reaper sees that demand, queues a proxy write turn so new checks wait in
//! the project's fair lock, and retires idle workers whose shared locks would
//! keep the writer out. A busy reader gate is contention, not proof of a
//! writer, so it changes nothing.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use lean_toolchain::writer_demand::ReadAdmission;

use super::ProjectBroker;
use crate::project_lock::Demand;

fn try_domains(domains: &[PathBuf]) -> io::Result<Option<Vec<ReadAdmission>>> {
    let mut paths = domains
        .iter()
        .map(|root| lean_toolchain::shared_writer::project_lock_path(root))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    let mut held = Vec::new();
    // Ordinary and companion generations can protect different domains. Keep
    // every chunk gated; never truncate their conservative lifetime union.
    for paths in paths.chunks(lean_toolchain::native_guardian::MAX_READ_DOMAINS) {
        let Some(admission) = ReadAdmission::try_acquire(paths)? else {
            return Ok(None);
        };
        held.push(admission);
    }
    Ok(Some(held))
}

impl ProjectBroker {
    /// Hold a proxy write turn on each project where another process's writer
    /// announced demand, until that demand ends. The domains are those guard G
    /// covers, plus every domain the project's live workers still protect.
    pub(crate) fn observe_writer_demand(&self, handle: &tokio::runtime::Handle) {
        let projects: Vec<_> = {
            let inner = self.inner.lock();
            inner
                .locks
                .iter()
                .map(|(root, lock)| {
                    let domains: BTreeSet<PathBuf> = inner
                        .registry
                        .iter()
                        .filter(|(key, _)| key.root == *root)
                        .flat_map(|(_, actor)| actor.protected_artifact_domains())
                        .collect();
                    (root.clone(), Arc::clone(lock), domains)
                })
                .collect()
        };
        for (root, lock, mut domains) in projects {
            // A root that is not a directory has no lock file to watch. A
            // manifest that does not parse leaves the root and worker domains.
            if root.is_dir() {
                if let Ok(roots) = lean_toolchain::shared_writer::project_artifact_roots(&root) {
                    domains.extend(roots);
                }
                domains.insert(root);
            }
            let demand = match try_domains(&domains.into_iter().collect::<Vec<_>>()) {
                Ok(Some(_)) => Demand::Absent,
                Ok(None) => Demand::Present,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Demand::Unknown,
                // A failed look must not keep a proxy turn held.
                Err(_) => Demand::Absent,
            };
            lock.observe_demand(demand, handle);
        }
    }

    /// Stop only an unborrowed idle actor whose exact physical domains contend.
    /// The existing retirement owner retains capacity until actual scope closure.
    pub(super) fn reap_artifact_handoffs(&self) {
        crate::worker_retirement::poll_retirements();
        let candidates: Vec<_> = self
            .inner
            .lock()
            .registry
            .iter()
            .filter(|(_, actor)| actor.is_idle() && Arc::strong_count(actor) == 1)
            .map(|(key, actor)| (key.clone(), Arc::downgrade(actor), actor.protected_artifact_domains()))
            .collect();
        for (key, identity, domains) in candidates {
            // Observe files outside the broker mutex. Failed observation also
            // retires an idle cache rather than admitting unverified new work.
            match try_domains(&domains) {
                Ok(Some(_)) => continue,
                // A concurrent fresh reader owns the brief exclusive gate.
                // Retry on the next tick without retiring a warm actor.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(None) | Err(_) => {}
            }
            let retired = {
                let mut inner = self.inner.lock();
                let eligible = inner.registry.get(&key).is_some_and(|actor| {
                    identity.ptr_eq(&Arc::downgrade(actor)) && actor.is_idle() && Arc::strong_count(actor) == 1
                });
                if eligible {
                    inner.last_used.remove(&key);
                    inner.registry.remove(&key)
                } else {
                    None
                }
            };
            if let Some(actor) = retired {
                tracing::debug!(project = %key.root.display(), lane = key.lane, "retiring idle worker for artifact handoff contention");
                actor.shutdown();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use std::fs::{self, File, OpenOptions};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use lean_toolchain::writer_demand::WriterDemand;

    use super::*;
    use crate::broker::BrokerConfig;
    use crate::error::ServerError;
    use crate::lake_meta::LakeProjectMeta;
    use crate::project::LeanProject;
    use crate::project_lock::EXTERNAL_WRITER;

    const WAIT: Duration = Duration::from_secs(5);

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        project: PathBuf,
        dependency: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let project = Self::project(&root, "Project");
            let dependency = Self::project(&root, "Dependency");
            Self {
                _directory: directory,
                root,
                project,
                dependency,
            }
        }
        fn project(root: &Path, name: &str) -> PathBuf {
            let root = root.join(name);
            fs::create_dir(&root).unwrap();
            fs::write(root.join("lakefile.lean"), format!("package {name}\nlean_lib {name}\n")).unwrap();
            fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.35.0\n").unwrap();
            fs::write(root.join("lake-manifest.json"), "{\"packages\":[]}\n").unwrap();
            root
        }
        fn broker(&self) -> Arc<ProjectBroker> {
            ProjectBroker::new(BrokerConfig {
                lake_root: Some(self.project.clone()),
                max_workers: std::num::NonZeroUsize::new(2).unwrap(),
                max_workers_per_project: std::num::NonZeroUsize::MIN,
                idle_timeout: Duration::ZERO,
            })
        }
        fn actor(root: &Path, domains: Vec<PathBuf>) -> Arc<LeanProject> {
            let files: Vec<File> = domains
                .iter()
                .map(|root| {
                    let path = lean_toolchain::shared_writer::project_lock_path(root).unwrap();
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(path)
                        .unwrap();
                    file.lock_shared().unwrap();
                    file
                })
                .collect();
            LeanProject::test_handoff_actor(
                root,
                LakeProjectMeta::from_explicit(root).unwrap().manifest_hash,
                domains,
                Box::new(files),
            )
        }
    }

    struct WriterChild {
        child: Child,
        root: PathBuf,
    }
    impl WriterChild {
        fn start(root: &Path, lock: &Path) -> Self {
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "broker::writer_handoff::tests::artifact_writer_child",
                ])
                .env("LEAN_HANDOFF_CHILD_ROOT", root)
                .env("LEAN_HANDOFF_CHILD_LOCK", lock)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            Self {
                child,
                root: root.to_path_buf(),
            }
        }
        async fn finish(mut self) {
            fs::write(self.root.join("writer-release"), "release").unwrap();
            wait_for(|| self.child.try_wait().unwrap().is_some()).await;
            assert!(self.child.wait().unwrap().success());
        }
    }
    impl Drop for WriterChild {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _kill_result = self.child.kill();
                let _wait_result = self.child.wait();
            }
        }
    }
    async fn wait_for(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bounded artifact handoff transition");
    }

    #[test]
    #[ignore = "internal real-child writer harness"]
    fn artifact_writer_child() {
        let root = PathBuf::from(std::env::var_os("LEAN_HANDOFF_CHILD_ROOT").expect("explicit child fixture"));
        let lock = PathBuf::from(std::env::var_os("LEAN_HANDOFF_CHILD_LOCK").expect("explicit lock domain"));
        let _demand = WriterDemand::announce(std::slice::from_ref(&lock), WAIT, &|| false).unwrap();
        fs::write(root.join("writer-demand"), "announced").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock)
            .unwrap();
        let end = Instant::now() + WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                outcome => panic!("artifact writer was not admitted: {outcome:?}"),
            }
        }
        fs::write(root.join("writer-admitted"), "exclusive").unwrap();
        while !root.join("writer-release").exists() {
            assert!(Instant::now() < end, "writer controller did not close");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn warm_frozen_dependency_hands_off_without_idle_expiry_or_manual_drain() {
        let f = Fixture::new();
        let broker = f.broker();
        let actor = Fixture::actor(&f.project, vec![f.project.clone(), f.dependency.clone()]);
        broker.test_register_actor(actor);
        let unrelated = Fixture::project(&f.root, "Unrelated");
        broker.test_register_actor(Fixture::actor(&unrelated, vec![unrelated.clone()]));
        // The current manifest never mentions Dependency: only the actual
        // retained physical domain can cause the correct actor to retire.
        let lock = lean_toolchain::shared_writer::project_lock_path(&f.dependency).unwrap();
        let writer = WriterChild::start(&f.root, &lock);
        wait_for(|| f.root.join("writer-admitted").exists()).await;
        assert!(!broker.resident_paths().contains(&f.project));
        let other = broker
            .select_worker(&unrelated)
            .unwrap()
            .expect("nonoverlapping actor remains reusable");
        assert!(other.is_healthy());
        drop(other);
        writer.finish().await;
        let _successor = WriterDemand::announce(&[lock], WAIT, &|| false).expect("successor demand after closure");
        broker.shutdown_all();
    }

    #[tokio::test]
    async fn admitted_call_finishes_before_handoff_and_new_calls_wait_behind_a_proxy_turn() {
        let f = Fixture::new();
        let broker = f.broker();
        let actor = Fixture::actor(&f.project, vec![f.project.clone(), f.dependency.clone()]);
        broker.test_register_actor(Arc::clone(&actor));
        let lock = broker.inner.lock().locks.get(&f.project);
        let writer = WriterChild::start(
            &f.root,
            &lean_toolchain::shared_writer::project_lock_path(&f.dependency).unwrap(),
        );
        wait_for(|| f.root.join("writer-demand").exists()).await;
        // The reaper queues a proxy write turn: new checks and idle work wait
        // in the project's fair lock instead of pinning the actor.
        wait_for(|| lock.try_read().is_none()).await;
        assert!(
            broker
                .test_maintenance_access()
                .try_begin(&f.project, "handoff-test")
                .is_none()
        );
        broker.reap_artifact_handoffs();
        assert!(
            actor.is_healthy(),
            "an admitted caller cannot be cancelled for writer demand"
        );
        assert!(!f.root.join("writer-admitted").exists());
        drop(actor); // the admitted call's existing Arc finishes normally
        wait_for(|| f.root.join("writer-admitted").exists()).await;
        writer.finish().await;
        wait_for(|| lock.try_read().is_some()).await;
        assert_eq!(
            broker.ensure_read(&f.project).await.unwrap(),
            1,
            "the proxy turn counts as one finished build"
        );
        broker.shutdown_all();
    }

    #[test]
    fn waits_behind_a_proxy_turn_keep_the_request_deadline_and_observe_cancellation_without_dispatch() {
        let f = Fixture::new();
        // Construct outside Tokio so no reaper runs; the test observes writer
        // demand itself.
        let broker = f.broker();
        broker.test_register_actor(Fixture::actor(&f.project, vec![f.project.clone()]));
        let lock = broker.inner.lock().locks.get(&f.project);
        let path = lean_toolchain::shared_writer::project_lock_path(&f.project).unwrap();
        let demand = WriterDemand::announce(&[path], WAIT, &|| false).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            broker.observe_writer_demand(&tokio::runtime::Handle::current());
            assert!(lock.try_read().is_none(), "writer demand queues a proxy turn");
            let request = tokio_util::sync::CancellationToken::new();
            let error = crate::cancellation::with_cancellation(
                &request,
                crate::cancellation::with_deadline(
                    Duration::from_millis(60),
                    Box::pin(broker.acquire(f.project.clone())),
                ),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                ServerError::WorkerUnavailable(info)
                    if info.reason == "project_admission_timeout"
                        && info.blocked_by.as_deref() == Some(EXTERNAL_WRITER)
            ));
            let cancellation = tokio_util::sync::CancellationToken::new();
            let wait = crate::cancellation::with_cancellation(&cancellation, broker.acquire(f.project.clone()));
            tokio::pin!(wait);
            tokio::select! {
                result = &mut wait => panic!("a call behind a proxy turn was admitted before cancellation: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            cancellation.cancel();
            let error = wait.await.unwrap_err();
            assert!(matches!(error, ServerError::WorkerUnavailable(info) if info.reason == "request_cancelled"));
            drop(demand);
            broker.observe_writer_demand(&tokio::runtime::Handle::current());
            wait_for(|| lock.try_read().is_some()).await;
        });
        assert_eq!(broker.resident_paths(), vec![f.project]);
        broker.shutdown_all();
    }

    #[test]
    fn conservative_multi_generation_union_gates_every_chunk_without_truncation() {
        let f = Fixture::new();
        let broker = f.broker();
        let mut domains = vec![f.project.clone()];
        for index in 0..34 {
            domains.push(Fixture::project(&f.root, &format!("Domain{index}")));
        }
        let mut locks: Vec<_> = domains
            .iter()
            .map(|root| lean_toolchain::shared_writer::project_lock_path(root).unwrap())
            .collect();
        locks.sort();
        broker.test_register_actor(Fixture::actor(&f.project, domains));
        let lock = broker.inner.lock().locks.get(&f.project);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            broker.observe_writer_demand(&tokio::runtime::Handle::current());
            assert!(lock.try_read().is_some(), "every historical domain can be gated");
            let last = locks.last().unwrap();
            let demand = WriterDemand::announce(std::slice::from_ref(last), WAIT, &|| false).unwrap();
            broker.observe_writer_demand(&tokio::runtime::Handle::current());
            assert!(
                lock.try_read().is_none(),
                "demand in the last chunk queues a proxy turn"
            );
            let gate = ReadAdmission::try_acquire(&locks[..32]).unwrap();
            assert!(
                gate.is_some(),
                "a later blocked chunk releases every earlier transient gate"
            );
            drop((gate, demand));
            broker.observe_writer_demand(&tokio::runtime::Handle::current());
            wait_for(|| lock.try_read().is_some()).await;
        });
        broker.shutdown_all();
    }
}
