//! Opt-in end-to-end test against a built Lake project with `lean-rs-host`
//! shims. v0.1 does not ship a bundled fixture (the shim contract is
//! non-trivial); point `LEAN_HOST_MCP_TEST_FIXTURE` at a built project to
//! enable.
//!
//! ```sh
//! cd /path/to/lean-rs/fixtures/lean && lake build
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
//!     LEAN_HOST_MCP_TEST_PACKAGE=lean_rs_fixture \
//!     LEAN_HOST_MCP_TEST_LIBRARY=LeanRsFixture \
//!     cargo test --test e2e -- --ignored
//! ```

// Test code: `expect`, `unwrap`, and `panic!` are the idiomatic way to
// surface test failures and unreachable setup branches.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use lean_host_mcp::SessionHost;

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn elaborate_prelude_term() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let result = host
        .elaborate("(Nat.succ 0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("elaborate returned");
    assert!(result.is_ok(), "elaboration should succeed: {result:?}");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn describe_prelude_name() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let row = host
        .describe("Nat.add_zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("describe");
    assert!(row.is_some(), "Nat.add_zero is part of the prelude");
}

#[test]
fn is_def_eq_request_round_trips_transparency() {
    use lean_host_mcp::tools::lean::{IsDefEqRequest, Transparency};

    let without: IsDefEqRequest = serde_json::from_str(r#"{"lhs":"1+1","rhs":"2"}"#).unwrap();
    assert!(without.transparency.is_none());

    let with: IsDefEqRequest = serde_json::from_str(r#"{"lhs":"1+1","rhs":"2","transparency":"reducible"}"#).unwrap();
    assert!(matches!(with.transparency, Some(Transparency::Reducible)));
}

#[test]
fn find_symbol_request_round_trips() {
    use lean_host_mcp::tools::index::FindSymbolRequest;

    let minimal: FindSymbolRequest = serde_json::from_str(r#"{"query":"add_zero"}"#).unwrap();
    assert_eq!(minimal.query, "add_zero");
    assert!(minimal.imports.is_empty());
    assert!(minimal.limit.is_none());

    let full: FindSymbolRequest = serde_json::from_str(r#"{"query":"map","imports":["List"],"limit":25}"#).unwrap();
    assert_eq!(full.limit, Some(25));
    assert_eq!(full.imports, vec!["List".to_owned()]);
}

#[test]
fn outline_request_accepts_module_prefix() {
    use lean_host_mcp::tools::index::OutlineRequest;

    let nat: OutlineRequest = serde_json::from_str(r#"{"module_prefix":"Nat."}"#).unwrap();
    assert_eq!(nat.module_prefix.as_deref(), Some("Nat."));

    let none: OutlineRequest = serde_json::from_str("{}").unwrap();
    assert!(none.module_prefix.is_none());
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn hover_by_name_populates_type_signature() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let row = host
        .describe("Nat.add_zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("describe")
        .expect("Nat.add_zero present");
    assert!(
        row.type_signature.is_some(),
        "expr_to_string_raw should yield a type for Nat.add_zero"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn index_rebuilds_and_finds_prelude_theorem() {
    use lean_host_mcp::{DeclarationIndex, fingerprint_lake_project};

    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let cache_dir = tempfile::tempdir().expect("tempdir");
    let host = SessionHost::spawn(root.clone(), pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let index = DeclarationIndex::open(cache_dir.path(), &root.to_string_lossy()).expect("open index");
    let fp = fingerprint_lake_project(&root).expect("fingerprint");
    let n = index
        .rebuild(&host, vec!["LeanRsFixture.Handles".into()], fp)
        .await
        .expect("rebuild");
    assert!(n > 100, "prelude should expose more than 100 names; got {n}");
    let hits = index.search_theorems("add_zero", 50).expect("search");
    assert!(
        hits.iter().any(|d| d.name == "Nat.add_zero"),
        "Nat.add_zero must be reachable through search_theorems"
    );
}

#[test]
fn envelope_serialises() {
    use lean_host_mcp::{Freshness, Response};
    let r = Response::ok(
        serde_json::json!({"foo": 1}),
        Freshness {
            lake_root: "/tmp/x".into(),
            imports: vec!["A.B".into()],
            session_id: "abc".into(),
            lean_toolchain: "leanprover/lean4:v4.29.1".into(),
        },
    );
    let s = serde_json::to_string(&r).unwrap();
    assert!(s.contains("\"foo\":1"));
    assert!(s.contains("\"lake_root\":\"/tmp/x\""));
    assert!(!s.contains("\"warnings\""), "empty warnings should be omitted");
}
