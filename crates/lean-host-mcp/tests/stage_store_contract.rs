//! Database-only reference store checks; not compiled into the server.
#![cfg(all(test, unix))]
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "disposable fixtures assert exact commit and crash observations"
)]

#[path = "../src/native_store/mod.rs"]
mod native_store;
use native_store::{connection, result_client, store};
#[path = "stage_store_contract/cases.rs"]
mod tests;
