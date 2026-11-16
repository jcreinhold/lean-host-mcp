//! The external writer client resolves exactly the Rust reader/writer lock domain.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn resolve(root: &Path, registry: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lean-host-mcp"))
        .arg("writer-lock-path")
        .arg(root)
        .env("LEAN_HOST_MCP_PROCESS_REGISTRY_DIR", registry)
        .env("LEAN_HOST_MCP_WORKERS_DIR", registry.join("missing-workers"))
        .env("LEAN_SYSROOT", registry.join("missing-sysroot"))
        .env("PATH", registry.join("missing-tools"))
        .output()
        .expect("invoke the read-only resolver")
}

fn descriptor(output: &Output) -> Value {
    assert!(output.status.success(), "resolver failed: {output:?}");
    assert!(
        output.stderr.is_empty(),
        "resolver wrote unexpected diagnostics: {output:?}"
    );
    assert!(output.stdout.len() <= 1024 * 1024, "descriptor exceeded frame limit");
    assert!(output.stdout.ends_with(b"\n"), "descriptor lacks its frame delimiter");
    assert_eq!(
        std::str::from_utf8(&output.stdout)
            .expect("UTF-8 JSON frame")
            .lines()
            .count(),
        1
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("one JSON descriptor");
    let object = result.as_object().expect("descriptor object");
    assert_eq!(object.len(), 3);
    assert_eq!(result["protocol"], 1);
    result
}

#[test]
fn resolver_matches_the_sdk_without_creating_runtime_state_or_a_lock() {
    let fixture = tempfile::tempdir().expect("private fixture");
    let parent = fixture.path().canonicalize().expect("canonical fixture");
    let root = parent.join("Project");
    fs::create_dir(&root).expect("create empty project directory");
    let registry = parent.join("must-not-create-registry");
    let expected = lean_toolchain::shared_writer::project_lock_path(&root).expect("SDK lock resolver");
    let result = descriptor(&resolve(&root, &registry));
    assert_eq!(result["project_root"], root.to_str().expect("UTF-8 fixture"));
    assert_eq!(result["lock_path"], expected.to_str().expect("UTF-8 lock path"));
    assert_eq!(expected.parent(), Some(parent.as_path()));
    assert!(!expected.exists(), "resolution created a lock file");
    assert!(!root.join(".lake").exists(), "resolution created build state");
    assert!(!registry.exists(), "resolution started server registration");
    assert_eq!(fs::read_dir(&root).expect("project entries").count(), 0);
    assert_eq!(fs::read_dir(&parent).expect("fixture entries").count(), 1);
}

#[test]
fn symlink_alias_resolves_to_the_same_canonical_project_domain() {
    let fixture = tempfile::tempdir().expect("private fixture");
    let parent = fixture.path().canonicalize().expect("canonical fixture");
    let root = parent.join("Project");
    let alias = parent.join("Alias");
    fs::create_dir(&root).expect("create project");
    std::os::unix::fs::symlink(&root, &alias).expect("create project alias");
    let registry = parent.join("must-not-create-registry");
    let direct = descriptor(&resolve(&root, &registry));
    let aliased = descriptor(&resolve(&alias, &registry));
    assert_eq!(aliased, direct);
    assert!(!Path::new(direct["lock_path"].as_str().expect("lock path")).exists());
    assert_eq!(fs::read_dir(&parent).expect("fixture entries").count(), 2);
}

#[test]
fn replacing_lake_keeps_the_same_external_lock_domain() {
    let fixture = tempfile::tempdir().expect("private fixture");
    let root = fixture.path().join("Project");
    let lake = root.join(".lake");
    fs::create_dir_all(&lake).expect("create initial build state");
    fs::write(lake.join("old"), b"old build state").expect("write initial build state");
    let registry = fixture.path().join("must-not-create-registry");
    let before = descriptor(&resolve(&root, &registry));
    fs::remove_dir_all(&lake).expect("remove old build state");
    fs::create_dir(&lake).expect("replace build state");
    fs::write(lake.join("new"), b"new build state").expect("write new build state");
    let after = descriptor(&resolve(&root, &registry));
    assert_eq!(after, before);
    assert_eq!(
        fs::read(lake.join("new")).expect("retained build state"),
        b"new build state"
    );
    assert!(!Path::new(after["lock_path"].as_str().expect("lock path")).exists());
    assert!(!registry.exists());
}

#[test]
fn missing_root_fails_without_a_success_descriptor_or_filesystem_side_effect() {
    let fixture = tempfile::tempdir().expect("private fixture");
    let root = fixture.path().join("Missing");
    let registry = fixture.path().join("must-not-create-registry");
    let output = resolve(&root, &registry);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writer-lock-path"));
    assert!(!root.exists());
    assert!(!registry.exists());
    assert_eq!(fs::read_dir(fixture.path()).expect("fixture entries").count(), 0);
}
