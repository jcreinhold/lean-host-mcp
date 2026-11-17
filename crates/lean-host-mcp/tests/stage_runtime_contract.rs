//! Real SDK child/guardian checks for the unwired stage reference adapter.
//!
//! These one-shot macOS tests exercise dummy processes, never the live server.
#![cfg(all(test, target_os = "macos"))]
#![forbid(unsafe_code)]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::significant_drop_tightening,
    reason = "isolated fixtures assert observations and retain custody until explicit group closure"
)]

#[path = "stage_runtime_contract/fixtures.rs"]
mod fixtures;
#[path = "../../../specs/native-execution/stage_kernel.rs"]
mod kernel;
#[path = "stage_runtime_contract/runtime.rs"]
mod runtime;
#[path = "stage_runtime_contract/cases.rs"]
mod tests;
