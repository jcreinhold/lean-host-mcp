//! Helpers the integration suites share: the fixture's Lean sysroot, the
//! installed worker the tests name, the chain project (`chain`), the HTTP
//! server harness (`http`), and the response sizes the smoke and performance
//! baselines report against.

// Each integration suite compiles its own copy of this module, so any item not
// used by *that* suite reads as dead there. Expected by construction.
#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use lean_host_mcp::ToolchainId;

pub(crate) mod chain;
#[cfg(unix)]
pub(crate) mod http;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The Lean sysroot and `lean-toolchain` pin the ignored toolchain tests use:
/// the fixture's pin from `LEAN_HOST_MCP_TEST_FIXTURE`, or the version of the
/// Lean at `LEAN_SYSROOT`.
pub(crate) fn sysroot_and_pin() -> TestResult<(PathBuf, String)> {
    let fixture_pin = std::env::var_os("LEAN_HOST_MCP_TEST_FIXTURE")
        .map(|root| fs::read_to_string(PathBuf::from(root).join("lean-toolchain")))
        .transpose()?;
    let sysroot = match std::env::var_os("LEAN_SYSROOT") {
        Some(root) => PathBuf::from(root),
        None => ToolchainId::parse(
            fixture_pin
                .as_deref()
                .ok_or("set LEAN_HOST_MCP_TEST_FIXTURE or LEAN_SYSROOT for this ignored test")?,
        )?
        .elan_dir()?,
    };
    let pin = if let Some(pin) = fixture_pin {
        pin.trim().to_owned()
    } else {
        let version = Command::new(sysroot.join("bin/lean")).arg("--version").output()?;
        if !version.status.success() {
            return Err(format!("lean --version failed: {version:?}").into());
        }
        let output = String::from_utf8(version.stdout)?;
        let version = output
            .split_once("version ")
            .and_then(|(_, rest)| rest.split_once(','))
            .map(|(version, _)| version)
            .ok_or("could not identify compiler version")?;
        format!("leanprover/lean4:v{version}")
    };
    Ok((sysroot, pin))
}

/// The installed worker directory that `LEAN_HOST_MCP_WORKERS_DIR` names, for
/// a test that starts workers.
///
/// The worker Cargo builds in `target/` refuses to serve: it needs the
/// `worker.json` and shims that `install-worker` puts beside it. Without the
/// variable the server looks under the Lake root's
/// `.local/lean-host-mcp/workers`, whose worker, if any, has no tie to the
/// tree under test: a suite can pass, or measure other behaviour, against a
/// worker built months earlier. One worker-memory measurement here was lost
/// that way.
pub(crate) fn installed_workers_dir() -> TestResult<PathBuf> {
    let dir =
        std::env::var_os("LEAN_HOST_MCP_WORKERS_DIR").ok_or("set LEAN_HOST_MCP_WORKERS_DIR to an installed worker")?;
    Ok(PathBuf::from(dir))
}

/// Lower end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MIN_BYTES: usize = 16 * 1024;

/// Upper end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MAX_BYTES: usize = 32 * 1024;

/// Default hard budget for ordinary model-facing MCP responses.
pub(crate) const MODEL_RESPONSE_HARD_BUDGET_BYTES: usize = 64 * 1024;
