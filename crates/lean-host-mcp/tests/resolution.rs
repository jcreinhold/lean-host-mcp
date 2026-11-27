//! [`ProjectBroker::resolve`] maps a call's `project` field to the one Lake
//! root a broker serves. These tests build fake Lake roots under a temporary
//! directory and open no worker.

#![allow(clippy::unwrap_used, reason = "tests assert the branch under test directly")]

use std::fs;
use std::path::{Path, PathBuf};

use lean_host_mcp::{BrokerConfig, LakeProjectMeta, ProjectBroker, ProjectHint, ServerError};

/// Build a fake Lake root containing a minimal `lakefile.lean`. Returns the
/// canonical path so tests can compare paths directly.
fn make_lake_root(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("lakefile.lean"), format!("package {name}\n")).unwrap();
    dir.canonicalize().unwrap()
}

fn refused(broker: &ProjectBroker, hint: &ProjectHint, reason: &str) -> bool {
    matches!(broker.resolve(hint), Err(ServerError::BadProject(message)) if message.contains(reason))
}

#[test]
fn default_hint_selects_the_served_root() {
    let tmp = tempfile::tempdir().unwrap();
    let served = make_lake_root(tmp.path(), "served");
    let broker = ProjectBroker::new(BrokerConfig::serving(served.clone()));
    assert_eq!(broker.resolve(&ProjectHint::Default).unwrap(), served);
}

#[test]
fn explicit_hint_naming_the_served_root_canonicalises() {
    let tmp = tempfile::tempdir().unwrap();
    let served = make_lake_root(tmp.path(), "served");
    let broker = ProjectBroker::new(BrokerConfig::serving(served.join(".")));
    let resolved = broker.resolve(&ProjectHint::Explicit(served.join("."))).unwrap();
    assert_eq!(resolved, served);
}

#[test]
fn explicit_hint_naming_another_root_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let served = make_lake_root(tmp.path(), "served");
    let other = make_lake_root(tmp.path(), "other");
    let broker = ProjectBroker::new(BrokerConfig::serving(served));
    assert!(refused(&broker, &ProjectHint::Explicit(other), "this server serves"));
}

#[test]
fn a_broker_without_a_root_refuses_every_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let project = make_lake_root(tmp.path(), "project");
    let broker = ProjectBroker::new(BrokerConfig {
        lake_root: None,
        ..BrokerConfig::serving(project.clone())
    });
    for hint in [ProjectHint::Default, ProjectHint::Explicit(project)] {
        assert!(refused(&broker, &hint, "serves no Lake project"));
    }
}

#[test]
fn unparseable_lakefile_toml_surfaces_through_from_explicit() {
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
