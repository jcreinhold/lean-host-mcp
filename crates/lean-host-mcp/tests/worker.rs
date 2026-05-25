//! Worker-backed [`LeanProject`] integration tests. Gated on
//! `LEAN_HOST_MCP_TEST_FIXTURE`—point at a built Lake fixture with the
//! `lean-rs-host` shims and run:
//!
//! ```sh
//! LEAN_HOST_MCP_TEST_FIXTURE=/path/to/lean-host-mcp/fixtures/lean \
//!     cargo test --test worker -- --ignored
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use lean_host_mcp::tools::ToolContext;
use lean_host_mcp::tools::lean::{
    ElaborateRequest, ElaborateResult, InferTypeRequest, KernelCheckRequest, elaborate, infer_type, kernel_check,
};
use lean_host_mcp::{BrokerConfig, ProjectBroker, Severity, default_cache_dir};

fn fixture_root() -> Option<PathBuf> {
    std::env::var("LEAN_HOST_MCP_TEST_FIXTURE").ok().map(PathBuf::from)
}

/// Build a broker that resolves to the fixture root through its
/// `env_default` slot. Tools still exercise the full broker dispatch
/// path — the same code production hits.
fn open_ctx() -> ToolContext {
    let Some(root) = fixture_root() else {
        panic!("LEAN_HOST_MCP_TEST_FIXTURE not set");
    };
    let broker = ProjectBroker::new(BrokerConfig {
        cache_dir: default_cache_dir(),
        config_default: None,
        env_default: Some(root.clone()),
        cwd: root,
        max_projects: BrokerConfig::default_max_projects(),
        idle_timeout: BrokerConfig::default_idle_timeout(),
    });
    ToolContext { broker }
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_spawns_against_fixture() {
    let ctx = open_ctx();
    let resp = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(Nat.succ 0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("elaborate completed");
    assert!(
        matches!(resp.result, ElaborateResult::Ok(_)),
        "elaboration should succeed: {:?}",
        resp.result
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_honors_per_call_imports() {
    // Open with empty defaults; per-call imports must still take effect.
    let ctx = open_ctx();
    let with_imports = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(Nat.succ 0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("with-imports call");
    assert!(
        matches!(with_imports.result, ElaborateResult::Ok(_)),
        "Nat.succ should elaborate under prelude: {:?}",
        with_imports.result
    );

    let with_other = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("second per-call import set");
    assert!(
        matches!(with_other.result, ElaborateResult::Ok(_)),
        "second elaborate should succeed: {:?}",
        with_other.result
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_kernel_check_populates_summary() {
    let ctx = open_ctx();
    let resp = kernel_check(
        &ctx,
        KernelCheckRequest {
            source: "theorem k_ok : 1 + 1 = 2 := rfl".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("kernel_check");
    let outcome = resp.result;
    assert_eq!(outcome.status, "Checked", "kernel must accept the proof: {outcome:?}");
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
    let ctx = open_ctx();
    let resp = infer_type(
        &ctx,
        InferTypeRequest {
            term: "Nat.succ Nat.zero".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("infer_type");
    let outcome = resp.result;
    assert_eq!(outcome.status, "Ok", "infer_type must succeed: {outcome:?}");
    assert!(outcome.rendered.is_some(), "rendered must carry a string");
    // The MetaOutcome's `raw_fallback_used` is a non-serialised internal
    // flag; the tool layer translates it into an envelope warning when set.
    // The fixture's capability dylib should ship `meta_pp_expr`, so the
    // warnings list must be empty for the standard rendered path.
    assert!(
        resp.warnings.is_empty(),
        "fixture capability should expose meta_pp_expr; got warnings: {:?}",
        resp.warnings
    );
}

#[tokio::test]
#[ignore = "requires a built Lake fixture; set LEAN_HOST_MCP_TEST_FIXTURE to enable"]
async fn worker_surfaces_elaboration_failure_in_ok_envelope() {
    let ctx = open_ctx();
    // Deliberately invalid term. The supervisor must not crash; the
    // failure must be in the `Failed` arm of the elaborate result.
    let resp = elaborate(
        &ctx,
        ElaborateRequest {
            source: "this_is_not_a_term".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("elaborate returned Ok");
    let ElaborateResult::Failed(failure) = resp.result else {
        panic!("invalid term must surface as Failed arm");
    };
    assert!(
        failure
            .diagnostics
            .iter()
            .any(|d| matches!(d.severity, Severity::Error)),
        "failure must include at least one error-severity diagnostic"
    );

    // Subsequent call still works—the worker is alive.
    let again = elaborate(
        &ctx,
        ElaborateRequest {
            source: "(0 : Nat)".into(),
            imports: vec!["LeanRsFixture.Handles".into()],
            project: None,
        },
    )
    .await
    .expect("follow-up call");
    assert!(
        matches!(again.result, ElaborateResult::Ok(_)),
        "worker still alive after surfacing failure"
    );
}
