//! Tool implementations.
//!
//! Public MCP calls enter through [`semantic`], the five-tool facade. The
//! remaining modules are internal operation building blocks, split by what
//! plumbing they share rather than one file per public tool:
//!
//! - [`declaration`]: `inspect_declaration`, the bounded single-declaration
//!   proof-work inspection tool.
//! - [`declaration_inventory`]: source-fresh and `.ilean`-backed declaration
//!   listings for `lean_lookup(kind = "declarations")`.
//! - [`proof_search`]: `search_for_proof`, the proof-agent retrieval tool
//!   built from bounded proof-state and declaration-search calls.
//! - [`proof_action`]: `try_proof_step` and `verify_declaration`, the
//!   non-mutating proof action tools.
//! - [`position`]: `proof_state` and `find_references`, backed by bounded
//!   module queries from `lean-rs-worker`.

use std::sync::Arc;

pub mod declaration;
pub mod declaration_inventory;
pub mod position;
pub mod proof_action;
pub mod proof_search;
pub mod semantic;
pub(crate) mod source_input;

use crate::broker::ProjectBroker;

/// Which field of the MCP tool result carries the serialized envelope.
///
/// The model reads `content` text; `structuredContent` serves code-mode /
/// validating clients. `Text` (the default) emits the envelope once, as a
/// `content` text block — the leanest shape for the proof-agent audience and
/// the only one Claude Code reads. `Both` duplicates onto both fields for
/// clients that want each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseCarrier {
    #[default]
    Text,
    Structured,
    Both,
}

impl ResponseCarrier {
    /// Parse the config/env spelling. Case-insensitive; returns `None` for an
    /// unknown value so the caller can report it.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "structured" => Some(Self::Structured),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// How much operational telemetry the model-facing envelope carries.
///
/// `Quiet` (the default) keeps only proof-relevant content: it drops the
/// `runtime` block (unless a restart/pressure signal is actionable), the
/// manifest hash and full import list from `freshness`, and per-tool search /
/// cache instrumentation. `Full` reproduces every field for debugging and
/// observability tooling. Correctness and truncation signals are never gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryVerbosity {
    #[default]
    Quiet,
    Full,
}

impl TelemetryVerbosity {
    /// Parse the config/env spelling. Case-insensitive; returns `None` for an
    /// unknown value so the caller can report it.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "quiet" => Some(Self::Quiet),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// Whether the demoted telemetry fields should be serialized.
    #[must_use]
    pub fn is_full(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Server-wide output budget overrides.
///
/// When unset, each tool keeps its own built-in byte caps (inspection allows a
/// larger per-field cap than the proof actions). When set, the value overrides
/// every tool's default before clamping, replacing what used to be a per-call
/// request argument.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputBudgetOverrides {
    pub max_field_bytes: Option<u32>,
    pub max_total_bytes: Option<u32>,
    pub heartbeat_limit: Option<u64>,
}

/// Presentation knobs resolved once at startup and shared by every tool call.
///
/// They decide where the envelope rides, how much telemetry it carries, and the
/// output byte/heartbeat budgets that were once per-call request arguments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolConfig {
    pub carrier: ResponseCarrier,
    pub verbosity: TelemetryVerbosity,
    pub output: OutputBudgetOverrides,
}

/// Shared state every tool handler reads.
///
/// Holds the broker and the resolved presentation [`ToolConfig`]; each tool
/// calls a narrow broker operation for its semantic work instead of receiving
/// a raw project actor handle.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub broker: Arc<ProjectBroker>,
    pub config: ToolConfig,
}

/// Imports passed to worker sessions: `Init` is the internal base
/// environment, while the public freshness envelope records only the
/// caller-supplied vector.
pub(crate) fn session_imports(imports: Vec<String>) -> Vec<String> {
    if imports.iter().any(|import| import == "Init") {
        imports
    } else {
        let mut out = Vec::with_capacity(imports.len().saturating_add(1));
        out.push("Init".to_owned());
        out.extend(imports);
        out
    }
}
