//! Resolution-chain unit tests for [`ProjectBroker::resolve`]. These
//! exercise the five-step chain (explicit → env → cwd-walk → config
//! default → error) without opening a Lean worker; they construct fake
//! Lake-root layouts under `tempfile::tempdir()`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use lean_host_mcp::{BrokerConfig, ProjectBroker, ProjectHint, ServerError};

/// Build a fake Lake-root directory containing a minimal `lakefile.lean`.
/// Returns the canonicalised path so tests can compare paths directly.
fn make_lake_root(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("lakefile.lean"),
        format!("package {name}\nlean_lib {}\n", name.replace('-', "_")),
    )
    .unwrap();
    dir.canonicalize().unwrap()
}

fn broker(cfg: BrokerConfig) -> std::sync::Arc<ProjectBroker> {
    ProjectBroker::new(cfg)
}

fn cfg(cwd: PathBuf, env_default: Option<PathBuf>, config_default: Option<PathBuf>) -> BrokerConfig {
    BrokerConfig {
        cache_dir: std::env::temp_dir(),
        config_default,
        env_default,
        cwd,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: std::time::Duration::ZERO,
    }
}

#[test]
fn explicit_hint_wins_over_env_default() {
    let tmp = tempfile::tempdir().unwrap();
    let explicit = make_lake_root(tmp.path(), "explicit_proj");
    let envroot = make_lake_root(tmp.path(), "env_proj");
    let b = broker(cfg(tmp.path().to_path_buf(), Some(envroot), None));
    let resolved = b.resolve(&ProjectHint::Explicit(explicit.clone())).unwrap();
    assert_eq!(resolved, explicit);
}

#[test]
fn env_default_wins_over_cwd_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd_proj = make_lake_root(tmp.path(), "cwd_proj");
    let env_proj = make_lake_root(tmp.path(), "env_proj");
    // cwd is inside cwd_proj (cwd-walk would otherwise find that root),
    // but env_default must short-circuit.
    let cwd = cwd_proj.join("sub");
    fs::create_dir_all(&cwd).unwrap();
    let b = broker(cfg(cwd, Some(env_proj.clone()), None));
    assert_eq!(b.resolve(&ProjectHint::Default).unwrap(), env_proj);
}

#[test]
fn cwd_walk_finds_lakefile_in_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = make_lake_root(tmp.path(), "proj");
    let nested = proj.join("a").join("b").join("c");
    fs::create_dir_all(&nested).unwrap();
    let b = broker(cfg(nested, None, None));
    assert_eq!(b.resolve(&ProjectHint::Default).unwrap(), proj);
}

#[test]
fn config_default_used_when_nothing_else_resolves() {
    let tmp = tempfile::tempdir().unwrap();
    let conf_proj = make_lake_root(tmp.path(), "conf_proj");
    // cwd is outside any Lake project.
    let bare = tmp.path().join("no_lakefile_here");
    fs::create_dir_all(&bare).unwrap();
    let b = broker(cfg(bare, None, Some(conf_proj.clone())));
    assert_eq!(b.resolve(&ProjectHint::Default).unwrap(), conf_proj);
}

#[test]
fn no_lakefile_anywhere_surfaces_bad_project() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("empty");
    fs::create_dir_all(&bare).unwrap();
    let b = broker(cfg(bare, None, None));
    let err = b.resolve(&ProjectHint::Default).unwrap_err();
    assert!(
        matches!(err, ServerError::BadProject(ref msg) if msg.contains("no lakefile found")),
        "expected BadProject('no lakefile found'); got {err:?}"
    );
}

#[test]
fn explicit_hint_canonicalises_relative_path() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = make_lake_root(tmp.path(), "rel_proj");
    let b = broker(cfg(tmp.path().to_path_buf(), None, None));
    // Pass a path containing `.` segments; canonicalisation must collapse them.
    let weird = proj.join(".");
    let resolved = b.resolve(&ProjectHint::Explicit(weird)).unwrap();
    assert_eq!(resolved, proj);
}

#[test]
fn unparseable_lakefile_toml_surfaces_through_from_explicit() {
    use lean_host_mcp::LakeProjectMeta;
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("broken");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("lakefile.toml"), "not valid toml { { {").unwrap();
    let err = LakeProjectMeta::from_explicit(&proj).unwrap_err();
    assert!(
        matches!(err, ServerError::BadProject(_)),
        "malformed lakefile.toml must surface as BadProject; got {err:?}"
    );
}
