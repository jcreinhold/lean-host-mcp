//! The `lean-host-mcp` server as a library, for its binary, tests and benches.
//!
//! The binary serves MCP over stdio, or over Streamable HTTP with `--bind`. A
//! call enters the rmcp glue in `server`, runs in [`tools`], and reaches Lean
//! through [`ProjectBroker`], which serves one Lake root and sends work to that
//! root's workers. The five semantic tools reply with `{data, errors, trust}`.
//! `docs/architecture.md` maps each module.

mod artifact_reads;
pub mod broker;
pub mod build;
mod cache;
mod cancellation;
pub mod cli;
mod config_file;
mod config_schema;
mod diagnosis;
mod digest;
mod envelope;
mod error;
mod host_preparation;
mod ilean;
mod lake_meta;
mod lake_options;
pub mod native_reader;
mod native_socket;
mod project;
mod project_lock;
mod projections;
mod request_log;
pub mod requests;
mod runtime_compatibility;
mod semantic_search;
mod server;
pub mod shared_writer;
mod smoke;
mod status;
pub mod store;
pub mod toolchain;
pub mod tools;
mod trust;
mod worker_retirement;

pub use broker::{BrokerConfig, ProjectBroker, ProjectHint};
pub use config_file::ConfigFile;
pub use envelope::ResponseStatus;
pub use error::{Result, ServerError};
pub use lake_meta::LakeProjectMeta;
pub use native_socket::directory as socket_directory;
pub use project::ProjectRuntimeConfig;
pub use projections::{
    CoordinateSpace, DeclarationInspectionResult, DeclarationSearchResult, DeclarationSearchTimings,
    DeclarationVerificationResult, ProofAttemptEnvelope, ProofAttemptResult, Severity,
};
pub use server::LeanHostService;
pub use toolchain::ToolchainId;
pub use tools::{OutputBudgetOverrides, ResponseCarrier, TelemetryVerbosity, ToolConfig};
pub use trust::{ArtifactKind, TrustScope, TrustStatus};
