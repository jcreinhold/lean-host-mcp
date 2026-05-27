//! Shared test-only constants for model-facing response budgets.
//!
//! These are observation thresholds for smoke/perf baselines. Production
//! truncation and enforcement belong to the tool-specific redesign prompts.

/// Lower end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MIN_BYTES: usize = 16 * 1024;

/// Upper end of the intended normal response-size range for model-controlled
/// MCP calls.
pub(crate) const MODEL_RESPONSE_TARGET_MAX_BYTES: usize = 32 * 1024;

/// Default hard budget for ordinary model-facing MCP responses.
pub(crate) const MODEL_RESPONSE_HARD_BUDGET_BYTES: usize = 64 * 1024;
