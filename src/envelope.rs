//! The uniform response envelope every tool returns.
//!
//! Encoding contract:
//!
//! ```jsonc
//! {
//!   "result":   { /* tool-specific */ },
//!   "freshness": {
//!     "lake_root":  "/abs/path",
//!     "imports":    ["Mod.A", "..."],
//!     "session_id": "uuid",
//!     "lean_toolchain": "leanprover/lean4:v4.x.y"
//!   },
//!   "warnings":     ["..."],     // omitted when empty
//!   "next_actions": ["..."]      // omitted when empty
//! }
//! ```
//!
//! This module hides three volatile decisions behind one shape: what
//! freshness means, how it's serialized, and what an MCP "warning" looks
//! like. Tools don't pick the layout; they build a `Response<T>` and let
//! rmcp serialize it.

use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Freshness {
    pub lake_root: String,
    pub imports: Vec<String>,
    pub session_id: String,
    pub lean_toolchain: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Response<T>
where
    T: Serialize + JsonSchema,
{
    pub result: T,
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
}

impl<T> Response<T>
where
    T: Serialize + JsonSchema,
{
    pub fn ok(result: T, freshness: Freshness) -> Self {
        Self {
            result,
            freshness,
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
    }

    #[must_use]
    pub fn warn(mut self, msg: impl Into<String>) -> Self {
        self.warnings.push(msg.into());
        self
    }

    #[must_use]
    pub fn hint(mut self, msg: impl Into<String>) -> Self {
        self.next_actions.push(msg.into());
        self
    }
}
