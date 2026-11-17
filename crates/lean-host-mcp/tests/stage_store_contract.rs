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

#[path = "stage_store_contract/connection.rs"]
mod connection;
#[path = "stage_store_contract/cuts.rs"]
mod cuts;
#[path = "stage_store_contract/result_client.rs"]
mod result_client;
#[path = "stage_store_contract/store.rs"]
mod store;
#[path = "stage_store_contract/cases.rs"]
mod tests;
use cuts::cut;
