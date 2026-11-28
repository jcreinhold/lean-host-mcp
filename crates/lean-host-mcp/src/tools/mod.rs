//! Tool implementations.
//!
//! Public MCP calls enter through [`semantic`], the five-tool facade. The
//! remaining modules are internal operation building blocks, split by what
//! plumbing they share rather than one file per public tool:
//!
//! - [`build`]: `lean_build` and `lean_imports`, which run Lake under the
//!   project's write turn and read what it built.
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

pub mod build;
pub mod declaration;
pub mod declaration_inventory;
pub mod position;
pub mod proof_action;
pub mod proof_search;
pub mod request_aliases;
pub mod semantic;
pub(crate) mod source_input;

use crate::broker::ProjectBroker;
use crate::config_file::{self, ConfigFile, Env, setting};

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
#[derive(Debug, Clone, Copy)]
pub struct ToolConfig {
    pub carrier: ResponseCarrier,
    pub verbosity: TelemetryVerbosity,
    pub output: OutputBudgetOverrides,
    /// Worker-call CPU milliseconds above which every response carries a
    /// slow-call refactoring warning. Default
    /// [`DEFAULT_SLOW_CALL_WARNING_MILLIS`].
    pub slow_call_warning_millis: u64,
}

/// Default [`ToolConfig::slow_call_warning_millis`]: ten seconds of worker CPU.
pub const DEFAULT_SLOW_CALL_WARNING_MILLIS: u64 = 10_000;

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            carrier: ResponseCarrier::default(),
            verbosity: TelemetryVerbosity::default(),
            output: OutputBudgetOverrides::default(),
            slow_call_warning_millis: DEFAULT_SLOW_CALL_WARNING_MILLIS,
        }
    }
}

impl ToolConfig {
    /// Read the presentation knobs: each is `env var > config file > default`.
    ///
    /// # Errors
    ///
    /// Names the env var or value that does not parse.
    pub fn from_env_with_file(file: &ConfigFile) -> Result<Self, String> {
        Self::resolve(&config_file::process_env, file)
    }

    /// [`Self::from_env_with_file`] with the env lookup passed in.
    fn resolve(env: Env<'_>, file: &ConfigFile) -> Result<Self, String> {
        let carrier = match setting(
            env,
            "LEAN_HOST_MCP_RESPONSE_CARRIER",
            file.server.response_carrier.clone(),
        )? {
            Some(value) => ResponseCarrier::parse(&value)
                .ok_or_else(|| format!("invalid response carrier {value:?}; expected text, structured, or both"))?,
            None => ResponseCarrier::default(),
        };
        let verbosity = match setting(
            env,
            "LEAN_HOST_MCP_TELEMETRY_VERBOSITY",
            file.telemetry.verbosity.clone(),
        )? {
            Some(value) => TelemetryVerbosity::parse(&value)
                .ok_or_else(|| format!("invalid telemetry verbosity {value:?}; expected quiet or full"))?,
            None => TelemetryVerbosity::default(),
        };
        let output = OutputBudgetOverrides {
            max_field_bytes: setting(env, "LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES", file.output.max_field_bytes)?,
            max_total_bytes: setting(env, "LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES", file.output.max_total_bytes)?,
            heartbeat_limit: setting(env, "LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT", file.output.heartbeat_limit)?,
        };
        let slow_call_warning_millis = setting(
            env,
            "LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS",
            file.output.slow_call_warning_millis,
        )?
        .unwrap_or(DEFAULT_SLOW_CALL_WARNING_MILLIS);
        Ok(Self {
            carrier,
            verbosity,
            output,
            slow_call_warning_millis,
        })
    }
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

/// Run `operation` as one tool call, as the server runs each call: the turns
/// it takes on a project end when it returns. For callers that run tool
/// functions in process, such as tests.
pub async fn as_call<T>(operation: impl Future<Output = T>) -> T {
    // The server boxes each call inside its scope too. Unboxed, each layer of
    // the scope keeps room for the whole future.
    crate::cancellation::with_cancellation(&tokio_util::sync::CancellationToken::new(), Box::pin(operation)).await
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    /// A set env var wins over the config file, and one set to the empty
    /// string counts as unset.
    #[test]
    fn tool_config_takes_env_over_file_and_skips_an_empty_env_var() -> Result<(), String> {
        let mut file = ConfigFile::default();
        file.telemetry.verbosity = Some("full".to_owned());
        file.output.max_field_bytes = Some(100);
        let empty = |name: &str| (name == "LEAN_HOST_MCP_TELEMETRY_VERBOSITY").then(OsString::new);
        assert_eq!(ToolConfig::resolve(&empty, &file)?.verbosity, TelemetryVerbosity::Full);
        let set = |name: &str| match name {
            "LEAN_HOST_MCP_TELEMETRY_VERBOSITY" => Some(OsString::from("quiet")),
            "LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES" => Some(OsString::from("50")),
            _ => None,
        };
        let config = ToolConfig::resolve(&set, &file)?;
        assert_eq!(config.verbosity, TelemetryVerbosity::Quiet);
        assert_eq!(config.output.max_field_bytes, Some(50));
        Ok(())
    }
}
