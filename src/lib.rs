//! Library surface for `lean-host-mcp`.
//!
//! The MCP server is one binary, but its pieces are crate-public for
//! integration tests and for downstream consumers who want to embed the
//! handlers in their own transport (e.g. SSE instead of stdio).
//!
//! Layout:
//!
//! - [`envelope`] — the uniform `{result, freshness, warnings, next_actions}`
//!   wrapper every tool returns.
//! - [`session`] — `SessionHost`, the single owner of all `lean-rs`
//!   `LeanRuntime` / `LeanHost` / `LeanCapabilities` / `LeanSession` state.
//!   Tools talk to it through a channel because `LeanSession` is `!Send`.
//! - [`index`] — `DeclarationIndex`, the SQLite-backed projection of the
//!   environment the three index tools query.
//! - [`error`] — `ServerError`, the one error type tool handlers return.
//! - [`tools`] — tool implementations, grouped by what plumbing they share
//!   (`lean` for session-backed tools, `scan` for the filesystem regex
//!   sweep, `index` for the SQLite-backed lookups).
//! - [`server`] — rmcp glue.

pub mod envelope;
pub mod error;
pub mod index;
pub mod server;
pub mod session;
pub mod tools;

pub use envelope::{Freshness, Response};
pub use error::{Result, ServerError};
pub use index::{DeclarationIndex, IndexedDeclaration, default_cache_dir, fingerprint_lake_project};
pub use server::LeanHostService;
pub use session::SessionHost;
