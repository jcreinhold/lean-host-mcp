//! Optional TOML config file holding every tunable knob.
//!
//! One file can set the runtime/worker, broker/pool, and server/transport
//! knobs that are otherwise `LEAN_HOST_MCP_*` env vars, plus the existing
//! `primary_project`. Discovery prefers a project-local `lean-host-mcp.toml`
//! (found by walking up from the invocation cwd, like the lakefile) and falls
//! back to the home file `<config-dir>/lean-host-mcp/config.toml`. When both
//! exist they merge **per key** (local overlays home), so a project file need
//! only restate the knobs it changes.
//!
//! The file is one layer in the precedence stack `CLI > env > file > default`:
//! every field here is `Option`, and a present env var still wins over a file
//! value (see `ProjectRuntimeConfig::from_env_with_file` /
//! `BrokerConfig::pool_from_env_with_file`). Missing files and parse failures
//! are non-fatal — a malformed file is logged and ignored, leaving env + the
//! built-in defaults in charge.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Project-local file name, discovered by walking up from the cwd. Also the
/// default destination `config init` writes to.
pub(crate) const LOCAL_FILE_NAME: &str = "lean-host-mcp.toml";

/// Merged view of the config file(s). All fields optional: an absent field
/// defers to the env var, then the built-in default.
#[derive(Debug, Default, Deserialize)]
pub struct ConfigFile {
    /// Default Lake project for calls without a `project=` argument. Kept for
    /// backward compatibility with the original `primary_project`-only file.
    pub primary_project: Option<PathBuf>,
    #[serde(default)]
    pub runtime: RuntimeFileConfig,
    #[serde(default)]
    pub broker: BrokerFileConfig,
    #[serde(default)]
    pub server: ServerFileConfig,
}

/// `[runtime]` — worker policy knobs (mirrors `ProjectRuntimeConfig`).
#[derive(Debug, Default, Deserialize)]
pub struct RuntimeFileConfig {
    pub worker_rss_post_job_restart_kib: Option<u64>,
    pub worker_rss_hard_kill_kib: Option<u64>,
    pub worker_rss_sample_millis: Option<u64>,
    pub import_switch_rss_soft_kib: Option<u64>,
    pub module_cache_rss_guard_kib: Option<u64>,
    pub module_cache_max_bytes: Option<u64>,
    pub project_mailbox_capacity: Option<usize>,
    pub worker_restart_limit: Option<usize>,
    pub worker_restart_window_secs: Option<u64>,
}

/// `[broker]` — project-pool and semantic-admission knobs.
#[derive(Debug, Default, Deserialize)]
pub struct BrokerFileConfig {
    pub max_projects: Option<usize>,
    pub idle_timeout_secs: Option<u64>,
    pub semantic_permits: Option<usize>,
    pub semantic_waiters: Option<usize>,
    pub semantic_admission_timeout_millis: Option<u64>,
}

/// `[server]` — transport knobs.
///
/// `lake_root` is intentionally absent: the project default lives in the
/// top-level `primary_project` key. `bind` is a raw string (e.g.
/// `"127.0.0.1:8765"`); the binary parses and validates it as a `SocketAddr`,
/// so this library schema stays free of transport types.
#[derive(Debug, Default, Deserialize)]
pub struct ServerFileConfig {
    pub bind: Option<String>,
    pub http_path: Option<String>,
}

impl ConfigFile {
    /// Load and merge the home and project-local files. Home is the base; the
    /// nearest `lean-host-mcp.toml` at or above `cwd` overlays it per key. A
    /// missing file contributes nothing; a malformed file is logged and
    /// skipped. Never fails: the worst case is an empty config (env + defaults).
    #[must_use]
    pub fn load(cwd: &Path) -> Self {
        let mut merged = toml::Value::Table(toml::Table::new());
        if let Some(home) = home_config_path()
            && let Some(value) = read_toml(&home)
        {
            merge_toml(&mut merged, value);
        }
        if let Some(local) = walk_up_for(cwd, LOCAL_FILE_NAME)
            && let Some(value) = read_toml(&local)
        {
            merge_toml(&mut merged, value);
        }
        match merged.try_into() {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!(error = %err, "merged config did not match the schema; ignoring it");
                Self::default()
            }
        }
    }
}

/// `<config-dir>/lean-host-mcp/config.toml`. `LEAN_HOST_MCP_CONFIG_DIR`
/// overrides the base dir (used by the test suite to avoid the developer's
/// real config). Shared with `config init --home`, so discovery and
/// generation target the same path.
pub(crate) fn home_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("LEAN_HOST_MCP_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)?;
    Some(base.join("lean-host-mcp").join("config.toml"))
}

/// Walk upward from `start` looking for `filename`; return the first match.
fn walk_up_for(start: &Path, filename: &str) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Read and parse one TOML file. Missing file → `None` (silent); parse error
/// → `None` with a warning, so a typo in one file can't take down startup.
fn read_toml(path: &Path) -> Option<toml::Value> {
    let contents = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<toml::Value>(&contents) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "config file parse failed; ignoring");
            None
        }
    }
}

/// Deep-merge `overlay` onto `base` in place: tables merge key-by-key
/// (recursing), any non-table value replaces wholesale. So `[runtime]` in a
/// local file overrides only the keys it sets, leaving sibling home keys.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                match base_table.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert the branch under test directly"
)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlays_local_over_home_per_key() {
        let mut base = toml::from_str::<toml::Value>(
            "primary_project = \"/home/proj\"\n[runtime]\nworker_rss_post_job_restart_kib = 5\n[broker]\nmax_projects = 8\n",
        )
        .unwrap();
        let overlay = toml::from_str::<toml::Value>(
            "[runtime]\nworker_rss_post_job_restart_kib = 8\nworker_rss_hard_kill_kib = 16\n",
        )
        .unwrap();
        merge_toml(&mut base, overlay);
        let config: ConfigFile = base.try_into().unwrap();

        // Local overrode the one runtime key it set...
        assert_eq!(config.runtime.worker_rss_post_job_restart_kib, Some(8));
        // ...added its own...
        assert_eq!(config.runtime.worker_rss_hard_kill_kib, Some(16));
        // ...and left untouched home keys (runtime sibling + other sections) intact.
        assert_eq!(config.broker.max_projects, Some(8));
        assert_eq!(config.primary_project.as_deref(), Some(Path::new("/home/proj")));
    }

    #[test]
    fn full_example_parses() {
        let config: ConfigFile = toml::from_str::<toml::Value>(
            "[runtime]\nworker_rss_post_job_restart_kib = 8388608\nworker_restart_window_secs = 60\n\
             [broker]\nmax_projects = 4\nsemantic_permits = 1\n\
             [server]\nbind = \"127.0.0.1:8765\"\nhttp_path = \"/mcp\"\n",
        )
        .unwrap()
        .try_into()
        .unwrap();

        assert_eq!(config.runtime.worker_rss_post_job_restart_kib, Some(8_388_608));
        assert_eq!(config.broker.max_projects, Some(4));
        assert_eq!(config.server.bind.as_deref(), Some("127.0.0.1:8765"));
        assert_eq!(config.server.http_path.as_deref(), Some("/mcp"));
    }

    #[test]
    fn empty_config_is_all_none() {
        let config = ConfigFile::default();
        assert!(config.primary_project.is_none());
        assert!(config.runtime.worker_rss_post_job_restart_kib.is_none());
        assert!(config.broker.max_projects.is_none());
        assert!(config.server.bind.is_none());
    }

    #[test]
    fn walk_up_finds_nearest_then_ancestor() {
        let tmp = std::env::temp_dir().join(format!("lhm-cfg-walk-{}", std::process::id()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.join(LOCAL_FILE_NAME), "max_projects_unused = 1\n").unwrap();
        // No file in nested or a/: walk-up should find the one at tmp.
        let found = walk_up_for(&nested, LOCAL_FILE_NAME).unwrap();
        assert_eq!(found, tmp.join(LOCAL_FILE_NAME));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
