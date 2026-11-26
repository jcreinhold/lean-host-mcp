#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::ArtifactReadGuard;

struct LockChild(Child);

impl LockChild {
    fn spawn(root: &Path, ready: &Path) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "artifact_reads::tests::exclusive_lock_child",
                "--ignored",
                "--nocapture",
            ])
            .env("LEAN_HOST_MCP_READ_TEST_ROOT", root)
            .env("LEAN_HOST_MCP_READ_TEST_READY", ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Self(child);
        let deadline = Instant::now().checked_add(Duration::from_secs(3)).unwrap();
        while !ready.is_file() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "lock child exited before admission"
            );
            assert!(Instant::now() < deadline, "lock child did not acquire EX");
            std::thread::sleep(Duration::from_millis(5));
        }
        child
    }

    fn release(&mut self) {
        self.0.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(self.0.wait().unwrap().success());
    }
}

impl Drop for LockChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }
}

#[test]
#[ignore = "owned subprocess fixture; invoked by regression_exclusive_child_prevents_artifact_reads"]
fn exclusive_lock_child() {
    let Some(root) = std::env::var_os("LEAN_HOST_MCP_READ_TEST_ROOT") else {
        return;
    };
    let ready = std::env::var_os("LEAN_HOST_MCP_READ_TEST_READY").unwrap();
    let path = lean_toolchain::shared_writer::project_lock_path(Path::new(&root)).unwrap();
    let lock = open(&path);
    lock.lock().unwrap();
    std::fs::write(ready, b"ready").unwrap();
    let mut input = [0];
    std::io::stdin().read_exact(&mut input).unwrap();
}

fn open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn stage_index(root: &Path) {
    let build = root.join(crate::lake_meta::BUILD_LIB_REL);
    std::fs::create_dir_all(&build).unwrap();
    std::fs::write(build.join("Demo.olean"), b"artifact").unwrap();
    std::fs::write(
        build.join("Demo.ilean"),
        br#"{"version":5,"module":"Demo","decls":{},"references":{}}"#,
    )
    .unwrap();
}

#[test]
fn regression_exclusive_child_prevents_artifact_reads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    stage_index(&root);
    let ready = temp.path().join("ready");
    let mut child = LockChild::spawn(&root, &ready);
    assert_eq!(
        ArtifactReadGuard::acquire(&root, Duration::from_millis(20))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    let declarations = crate::ilean::declarations_in_module(&root, "Demo");
    assert_eq!(
        declarations.status,
        crate::ilean::ModuleDeclarationIndexStatus::Unavailable
    );
    assert!(declarations.declarations.is_empty());
    let references = crate::ilean::references_to(&root, "Demo.foo", None);
    assert_eq!(references.status, crate::ilean::IndexStatus::Unavailable);
    assert_eq!(references.modules_scanned, 0);
    child.release();
    assert_eq!(
        crate::ilean::declarations_in_module(&root, "Demo").status,
        crate::ilean::ModuleDeclarationIndexStatus::Present
    );
    assert_eq!(
        crate::ilean::references_to(&root, "Demo.foo", None).status,
        crate::ilean::IndexStatus::Present
    );
}

#[test]
fn parent_read_guard_holds_complete_dependency_union_until_drop() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let dependency = temp.path().join("dependency");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&dependency).unwrap();
    std::fs::write(
        dependency.join("lake-manifest.json"),
        r#"{"version":"1.1.0","packages":[]}"#,
    )
    .unwrap();
    std::fs::write(root.join("lake-manifest.json"), serde_json::to_vec(&serde_json::json!({
        "version":"1.1.0", "packages":[{"name":"dependency","type":"path","dir":dependency,"manifestFile":"lake-manifest.json"}]
    })).unwrap()).unwrap();
    let guard = ArtifactReadGuard::try_acquire(&root).unwrap();
    for root in [&root, &dependency] {
        let file = open(&lean_toolchain::shared_writer::project_lock_path(root).unwrap());
        assert!(matches!(file.try_lock(), Err(TryLockError::WouldBlock)));
    }
    drop(guard);
    for root in [&root, &dependency] {
        let file = open(&lean_toolchain::shared_writer::project_lock_path(root).unwrap());
        // A concurrent test's fork may briefly inherit an OFD before its exec
        // closes CLOEXEC descriptors. No reader transfers or retains a loan.
        let deadline = Instant::now().checked_add(Duration::from_millis(250)).unwrap();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                outcome => panic!("parent read descriptors did not close: {outcome:?}"),
            }
        }
    }
}

#[test]
fn unsupported_dependency_closure_refuses_parent_reads() {
    let root = tempfile::tempdir().unwrap();
    stage_index(root.path());
    std::fs::write(root.path().join("lakefile.lean"), "import Lake").unwrap();
    let error = ArtifactReadGuard::try_acquire(root.path()).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("no dependency manifest"));
    assert_eq!(
        crate::ilean::references_to(root.path(), "Demo.foo", None).status,
        crate::ilean::IndexStatus::Unavailable
    );
}

#[cfg(unix)]
#[test]
fn artifact_symlink_escape_refuses_parent_reads() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    stage_index(outside.path());
    std::fs::create_dir_all(root.path().join(".lake/build/lib")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join(crate::lake_meta::BUILD_LIB_REL),
        root.path().join(crate::lake_meta::BUILD_LIB_REL),
    )
    .unwrap();
    let guard = ArtifactReadGuard::try_acquire(root.path()).unwrap();
    assert!(!guard.covers_artifact_root(&root.path().join(crate::lake_meta::BUILD_LIB_REL)));
    assert_eq!(
        crate::ilean::declarations_in_module(root.path(), "Demo").status,
        crate::ilean::ModuleDeclarationIndexStatus::Unavailable
    );
    assert_eq!(
        crate::ilean::references_to(root.path(), "Demo.foo", None).status,
        crate::ilean::IndexStatus::Unavailable
    );
}

#[test]
fn malformed_committed_writer_evidence_refuses_parent_reads() {
    let root = tempfile::tempdir().unwrap();
    let path = lean_toolchain::shared_writer::project_lock_path(root.path()).unwrap();
    std::fs::write(path, b"{}\n").unwrap();
    let error = ArtifactReadGuard::try_acquire(root.path()).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("no exact owner"));
}

#[cfg(unix)]
#[test]
#[allow(
    clippy::used_underscore_binding,
    reason = "the test inspects custody descriptors retained only for drop"
)]
fn parent_read_descriptors_are_close_on_exec() {
    let root = tempfile::tempdir().unwrap();
    let guard = ArtifactReadGuard::try_acquire(root.path()).unwrap();
    assert!(guard._locks.iter().all(|file| {
        rustix::io::fcntl_getfd(file)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    }));
}

#[cfg(unix)]
#[test]
fn undeclared_nested_workspace_symlink_refuses_parent_reads() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    stage_index(&nested);
    std::fs::create_dir_all(root.path().join(".lake/build/lib")).unwrap();
    std::os::unix::fs::symlink(
        nested.join(crate::lake_meta::BUILD_LIB_REL),
        root.path().join(crate::lake_meta::BUILD_LIB_REL),
    )
    .unwrap();
    let guard = ArtifactReadGuard::try_acquire(root.path()).unwrap();
    assert!(!guard.covers_artifact_root(&root.path().join(crate::lake_meta::BUILD_LIB_REL)));
    assert_eq!(
        crate::ilean::references_to(root.path(), "Demo.foo", None).status,
        crate::ilean::IndexStatus::Unavailable
    );
}
