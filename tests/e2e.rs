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
    let (root, pkg, lib) = match fixture_env() {
        Some(t) => t,
        None => panic!("LEAN_HOST_MCP_TEST_FIXTURE not set"),
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
    let (root, pkg, lib) = match fixture_env() {
        Some(t) => t,
        None => panic!("LEAN_HOST_MCP_TEST_FIXTURE not set"),
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let row = host
        .describe("Nat.add_zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("describe");
    assert!(row.is_some(), "Nat.add_zero is part of the prelude");
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
