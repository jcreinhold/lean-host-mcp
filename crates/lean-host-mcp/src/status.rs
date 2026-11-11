//! Stable build identity and resolved settings for operational status.

use schemars::JsonSchema;
use serde::Serialize;

/// Compile-time source identity, never inferred from the live checkout.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BuildIdentity {
    pub version: &'static str,
    /// Full Git commit, or null for a source archive without Git metadata.
    pub revision: Option<&'static str>,
    /// Whether the source tree was modified at build time; null if unknown.
    pub dirty: Option<bool>,
}

impl Default for BuildIdentity {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            revision: option_env!("LEAN_HOST_MCP_BUILD_REVISION"),
            dirty: option_env!("LEAN_HOST_MCP_BUILD_DIRTY").and_then(|value| value.parse().ok()),
        }
    }
}

/// Retention accounting, not proof that a client has consumed its final result.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RequestRegistryStatus {
    pub active: usize,
    pub retained_terminal: usize,
    pub capacity: usize,
    pub terminal_retention_millis: u64,
}

/// Resolved execution controls, not raw environment or configuration files.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EffectiveSettings {
    pub broker: BrokerSettings,
    pub runtime: RuntimeSettings,
    /// Filled by the tool service; absent when calling the broker directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerSettings>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrokerSettings {
    pub max_projects: usize,
    pub idle_timeout_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RuntimeSettings {
    pub lean_max_memory_kib: u64,
    pub request_timeout_millis: u64,
    pub project_mailbox_capacity: usize,
    pub worker_restart_limit: usize,
    pub worker_restart_window_secs: u64,
    pub worker_import_residue_budget_bytes: u64,
    pub worker_import_residue_soft_bytes: u64,
    pub worker_session_pool_capacity: usize,
}

impl From<&crate::ProjectRuntimeConfig> for RuntimeSettings {
    fn from(config: &crate::ProjectRuntimeConfig) -> Self {
        Self {
            lean_max_memory_kib: config.lean_max_memory_kib(),
            request_timeout_millis: config.request_timeout_millis(),
            project_mailbox_capacity: config.mailbox_capacity(),
            worker_restart_limit: config.max_restarts_per_window(),
            worker_restart_window_secs: config.restart_window().as_secs(),
            worker_import_residue_budget_bytes: config.import_residue_budget_bytes(),
            worker_import_residue_soft_bytes: config.import_residue_soft_bytes(),
            worker_session_pool_capacity: config.session_pool_capacity(),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ServerSettings {
    pub response_carrier: &'static str,
    pub telemetry_verbosity: &'static str,
    /// Null overrides preserve each tool's own default output budget.
    pub output_overrides: OutputOverrides,
    pub slow_call_warning_millis: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OutputOverrides {
    pub max_field_bytes: Option<u32>,
    pub max_total_bytes: Option<u32>,
    pub heartbeat_limit: Option<u64>,
}

impl From<crate::ToolConfig> for ServerSettings {
    fn from(config: crate::ToolConfig) -> Self {
        use crate::{ResponseCarrier, TelemetryVerbosity};
        Self {
            response_carrier: match config.carrier {
                ResponseCarrier::Text => "text",
                ResponseCarrier::Structured => "structured",
                ResponseCarrier::Both => "both",
            },
            telemetry_verbosity: match config.verbosity {
                TelemetryVerbosity::Quiet => "quiet",
                TelemetryVerbosity::Full => "full",
            },
            output_overrides: OutputOverrides {
                max_field_bytes: config.output.max_field_bytes,
                max_total_bytes: config.output.max_total_bytes,
                heartbeat_limit: config.output.heartbeat_limit,
            },
            slow_call_warning_millis: config.slow_call_warning_millis,
        }
    }
}
