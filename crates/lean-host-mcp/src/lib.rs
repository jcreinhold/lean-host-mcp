//! Library surface for `lean-host-mcp`.
//!
//! The MCP server is one binary, but its pieces are crate-public for
//! integration tests and for downstream consumers who want to embed the
//! handlers in their own transport (e.g. SSE instead of stdio).
//!
//! Layout:
//!
//! - [`envelope`]: the uniform
//!   `{result, freshness, runtime, warnings, next_actions}` wrapper every
//!   semantic tool returns.
//! - [`broker`]: `ProjectBroker`, the mediator that resolves a per-call
//!   project hint into typed operations on a private project runtime via the
//!   env / cwd-walk / config-default chain.
//! - `project`: private per-project actor runtime. Owns one supervised worker
//!   actor and the bounded module-query cache for one Lake project.
//! - [`projections`]: pure data-shuffle projection types and helpers from
//!   `lean-rs-worker` shapes into the wire shapes the MCP envelope carries.
//! - [`lake_meta`]: `LakeProjectMeta`, the minimal description of a Lake
//!   project that the private project runtime consumes.
//! - [`error`]: `ServerError`, the one error type tool handlers return.
//! - [`tools`]: tool implementations, grouped by proof workflow stage.
//! - [`server`]: rmcp glue.

mod admission;
pub mod broker;
mod cache;
pub mod cli;
pub mod config_file;
pub mod config_schema;
mod diagnosis;
pub mod envelope;
pub mod error;
mod ilean;
pub mod lake_meta;
mod project;
pub mod projections;
pub mod server;
mod smoke;
pub mod toolchain;
pub mod tools;

pub use broker::{BrokerConfig, ProjectBroker, ProjectHint};
pub use config_file::ConfigFile;
pub use envelope::{Freshness, Response, ResponseStatus, RuntimeFacts, RuntimeFailure};
pub use error::{Result, ServerError};
pub use lake_meta::LakeProjectMeta;
pub use project::ProjectRuntimeConfig;
pub use projections::{
    DeclarationFlags, DeclarationInspection, DeclarationInspectionResult, DeclarationProofSearchFacts, DeclarationRow,
    DeclarationSearchFacts, DeclarationSearchPruning, DeclarationSearchResult, DeclarationSearchTimings,
    DeclarationSummary, DeclarationVerificationFacts, DeclarationVerificationResult, Diagnostic, ElabFailure,
    ElabSuccess, KernelOutcome, KernelSummary, MetaOutcome, ModuleSourceSpan, Position, ProofActionDeclarationTarget,
    ProofAttemptCandidate, ProofAttemptEnvelope, ProofAttemptResult, RenderedText, Severity, SourceRange,
};
pub use server::LeanHostService;
pub use toolchain::{ToolchainError, ToolchainId, WorkerBinary};
pub use tools::{OutputBudgetOverrides, ResponseCarrier, TelemetryVerbosity, ToolConfig};
