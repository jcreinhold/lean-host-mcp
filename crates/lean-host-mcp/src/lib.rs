//! Library surface for `lean-host-mcp`.
//!
//! The MCP server is one binary, but its pieces are crate-public for
//! integration tests and for downstream consumers who want to embed the
//! handlers in their own transport (e.g. SSE instead of stdio).
//!
//! Layout:
//!
//! - [`envelope`]: the uniform `{result, freshness, warnings, next_actions}`
//!   wrapper every tool returns.
//! - [`broker`]: `ProjectBroker`, the mediator that resolves a per-call
//!   project hint into an `Arc<LeanProject>` via the env / cwd-walk /
//!   config-default chain.
//! - [`project`]: `LeanProject`, the unit of multiplexing. Bundles the
//!   shims-only worker host handle, the `SQLite` declaration index, and the
//!   in-memory processed-file cache for one Lake project.
//! - [`projections`]: pure data-shuffle projection types and helpers from
//!   `lean-rs-worker` shapes into the wire shapes the MCP envelope carries.
//! - [`lake_meta`]: `LakeProjectMeta`, the minimal description of a Lake
//!   project that `LeanProject::open` consumes.
//! - [`index`]: `DeclarationIndex`, the SQLite-backed projection of the
//!   environment the three index tools query.
//! - [`error`]: `ServerError`, the one error type tool handlers return.
//! - [`tools`]: tool implementations, grouped by what plumbing they share
//!   (`lean` for session-backed tools, `scan` for the filesystem regex
//!   sweep, `index` for the SQLite-backed lookups).
//! - [`server`]: rmcp glue.

pub mod broker;
pub mod cache;
pub mod cli;
pub mod envelope;
pub mod error;
pub mod index;
pub mod lake_meta;
pub mod project;
pub mod projections;
pub mod server;
pub mod toolchain;
pub mod tools;

pub use broker::{BrokerConfig, ProjectBroker, ProjectHint};
pub use envelope::{Freshness, Response};
pub use error::{Result, ServerError};
pub use index::{DeclarationIndex, IndexedDeclaration, default_cache_dir, fingerprint_lake_project};
pub use lake_meta::LakeProjectMeta;
pub use project::LeanProject;
pub use projections::{
    DeclarationRow, Diagnostic, ElabFailure, ElabSuccess, KernelOutcome, KernelSummary, MetaOutcome, Position,
    ProcessedFile, Severity, SourceRange,
};
pub use server::LeanHostService;
pub use toolchain::{ToolchainError, ToolchainId, WorkerBinary};
