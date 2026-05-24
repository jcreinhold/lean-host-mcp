//! Worker-backed `SessionHost` integration tests. Gated on
//! `LEAN_HOST_MCP_TEST_FIXTURE` — point at a built Lake fixture with the
//! `lean-rs-host` shims and run:
//!
//! ```sh
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-rs/fixtures/lean \
//!     cargo test --test worker -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use lean_host_mcp::SessionHost;
use lean_host_mcp::session::Severity;

fn fixture_env() -> Option<(PathBuf, String, String)> {
    let root = std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok()?;
    let pkg = std::env::var("LEAN_HOST_MCP_TEST_PACKAGE").unwrap_or_else(|_| "lean_rs_fixture".into());
    let lib = std::env::var("LEAN_HOST_MCP_TEST_LIBRARY").unwrap_or_else(|_| "LeanRsFixture".into());
    Some((PathBuf::from(root), pkg, lib))
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_spawns_against_fixture() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let result = host
        .elaborate("(Nat.succ 0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("elaborate completed");
    assert!(result.is_ok(), "elaboration should succeed: {result:?}");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_honors_per_call_imports() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    // Spawn with an empty default; per-call imports must still take effect.
    let host = SessionHost::spawn(root, pkg, lib, Vec::new()).expect("spawn");
    let with_imports = host
        .elaborate("(Nat.succ 0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("with-imports call");
    assert!(
        with_imports.is_ok(),
        "Nat.succ should elaborate under prelude: {with_imports:?}"
    );

    // Different import set — must still round-trip cleanly without a stale
    // session leaking from the previous call.
    let with_other = host
        .elaborate("(0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("second per-call import set");
    assert!(with_other.is_ok(), "second elaborate should succeed: {with_other:?}");
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_kernel_check_populates_summary() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let outcome = host
        .kernel_check(
            "theorem k_ok : 1 + 1 = 2 := rfl".into(),
            vec!["LeanRsFixture.Handles".into()],
        )
        .await
        .expect("kernel_check");
    assert_eq!(outcome.status, "Checked", "kernel must accept the proof: {outcome:?}");
    // 0.1.7 regression gate: summary must be Some on a Checked result.
    let summary = outcome.summary.expect("0.1.7 must populate summary on Checked");
    assert_eq!(summary.declaration_name, "k_ok");
    assert!(
        !summary.type_signature.is_empty(),
        "summary must carry a pretty-printed type signature"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_infer_type_marks_rendering_provenance() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    let outcome = host
        .infer_type("Nat.succ Nat.zero".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("infer_type");
    assert_eq!(outcome.status, "Ok", "infer_type must succeed: {outcome:?}");
    assert!(outcome.rendered.is_some(), "rendered must carry a string");
    // 0.1.7 gate: rendering provenance reaches the MCP layer. The fixture's
    // capability dylib should ship `meta_pp_expr`, so the flag must be
    // false. If the fixture deliberately omits it, this assertion will
    // catch the drift.
    assert!(
        !outcome.raw_fallback_used,
        "fixture capability should expose meta_pp_expr; raw fallback used"
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_surfaces_elaboration_failure_in_ok_envelope() {
    let Some((root, pkg, lib)) = fixture_env() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let host = SessionHost::spawn(root, pkg, lib, vec!["LeanRsFixture.Handles".into()]).expect("spawn");
    // Deliberately invalid term. The supervisor must not crash; the
    // failure must be in the `Err` arm of the Ok envelope.
    let result = host
        .elaborate("this_is_not_a_term".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("elaborate returned Ok");
    let failure = result.expect_err("invalid term must surface as Err arm");
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| matches!(d.severity, Severity::Error)),
        "failure must include at least one error-severity diagnostic"
    );

    // Subsequent call still works — the worker is alive.
    let again = host
        .elaborate("(0 : Nat)".into(), vec!["LeanRsFixture.Handles".into()])
        .await
        .expect("follow-up call");
    assert!(again.is_ok(), "worker still alive after surfacing failure");
}
