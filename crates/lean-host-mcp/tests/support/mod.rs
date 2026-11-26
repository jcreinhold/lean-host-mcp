//! Shared test-only constants for model-facing response budgets.
//!
//! These are observation thresholds for smoke/perf baselines. Production
//! truncation and enforcement belong to the tool-specific redesign prompts.

// Each integration suite compiles its own copy of this module, so any item not
// used by *that* suite reads as dead there. Expected by construction.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lean_host_mcp::ToolchainId;

pub(crate) mod chain;

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

/// The worker directory a spawned test server must be pinned to.
///
/// Without `LEAN_HOST_MCP_WORKERS_DIR` the parent resolves the worker from the
/// developer's *installed* set under `~/Library/Application Support` (or
/// `$XDG_DATA_HOME`), which has no relationship to the tree under test: a suite
/// can pass, or silently measure entirely different behaviour, against a worker
/// built months earlier. That is not hypothetical — a worker-memory measurement
/// here was invalidated exactly that way, because the installed binary predated
/// the session-reuse fix it was supposed to be exercising.
///
/// Derived from `CARGO_BIN_EXE_lean-host-mcp` so it follows the profile the
/// tests were built in; `target/debug` hardcoded is wrong under
/// `cargo test --release`.
///
/// `None` when that profile has no worker built — `install-worker` moves it out
/// of `target/`, and not every checkout builds it. Callers then leave
/// resolution alone rather than pinning at an empty directory, so the installed
/// worker still serves the suites that only need *a* worker.
pub(crate) fn built_workers_dir(parent_binary: &str) -> Option<PathBuf> {
    let dir = Path::new(parent_binary).parent()?;
    dir.join("lean-host-mcp-worker").is_file().then(|| dir.to_path_buf())
}

/// Lower end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MIN_BYTES: usize = 16 * 1024;

/// Upper end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MAX_BYTES: usize = 32 * 1024;

/// Default hard budget for ordinary model-facing MCP responses.
pub(crate) const MODEL_RESPONSE_HARD_BUDGET_BYTES: usize = 64 * 1024;
