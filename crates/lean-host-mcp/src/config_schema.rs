//! The one catalogue of every configuration knob.
//!
//! A knob is defined once, here, as a [`FieldDoc`] row in [`SCHEMA_FIELDS`]:
//! its TOML key, type, default, the env var / CLI flag that overrides it, and
//! a one-line description. Two renderers consume that catalogue —
//! [`render_default_toml`] writes the documented starter file `config init`
//! produces, and [`render_reference_table`] writes the Markdown table embedded
//! in `docs/operations.md`. Neither output is hand-maintained, so the file, the
//! docs, and the code can't drift from one another. A test ties each row's
//! default to the built-in constant it documents, and another asserts the
//! committed docs table matches what this module renders.
//!
//! The schema *structs* and discovery/merge live next door in
//! [`crate::config_file`]; this module is their human-facing companion.

/// One configuration knob: everything a reader or a generator needs to know
/// about it, in one place.
struct FieldDoc {
    /// Dotted TOML path, e.g. `"runtime.worker_rss_post_job_restart_kib"`. A
    /// key with no `.` is a top-level key (only `primary_project`).
    key: &'static str,
    /// Human-readable type and unit, e.g. `"integer (KiB)"`.
    ty: &'static str,
    /// The value shown in the generated file, as a TOML literal (string
    /// literals include their quotes). For a knob with a built-in default this
    /// is that default; for an optional knob it is an illustrative example.
    value: &'static str,
    /// Whether the generated file comments the line out. `true` for optional
    /// knobs that have no built-in default (their absence selects a behaviour —
    /// stdio, cwd-resolved project), so a fresh file changes nothing until the
    /// user opts in.
    commented: bool,
    /// The env var, and CLI flag where one exists, that override this knob.
    /// Precedence is CLI flag > env var > file > built-in default.
    overrides: &'static str,
    /// One-line description. Single line so it serves both the file comment and
    /// the Markdown table cell; must not contain `|`.
    description: &'static str,
}

/// Every knob, in the order it appears in the generated file and docs table:
/// the top-level project default, then `[runtime]`, `[broker]`, `[server]`.
const SCHEMA_FIELDS: &[FieldDoc] = &[
    FieldDoc {
        key: "primary_project",
        ty: "path",
        value: "\"/abs/path/to/lake/project\"",
        commented: true,
        overrides: "--lake-root / LEAN_HOST_MCP_PROJECT",
        description: "Default Lake project for calls that omit an explicit project= argument. Lowest-priority fallback, after the flag/env and the nearest lakefile above the working directory.",
    },
    // ---- [runtime] — worker memory + lifecycle --------------------------
    FieldDoc {
        key: "runtime.worker_rss_post_job_restart_kib",
        ty: "integer (KiB)",
        value: "5242880",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB",
        description: "Post-job soft restart ceiling: after a call finishes, the worker is recycled if its resident memory is at or above this. Raise toward the hard-kill ceiling to recycle less often. Default 5 GiB.",
    },
    FieldDoc {
        key: "runtime.worker_rss_hard_kill_kib",
        ty: "integer (KiB)",
        value: "16777216",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_RSS_HARD_KILL_KIB",
        description: "In-flight hard-kill ceiling: a call whose worker crosses this is killed mid-call so a runaway tactic cannot exhaust the machine. Must be at least the post-job ceiling. Default 16 GiB.",
    },
    FieldDoc {
        key: "runtime.worker_rss_sample_millis",
        ty: "integer (ms)",
        value: "250",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_RSS_SAMPLE_MILLIS",
        description: "How often the supervisor samples worker resident memory for the in-flight hard-kill watchdog.",
    },
    FieldDoc {
        key: "runtime.import_switch_rss_soft_kib",
        ty: "integer (KiB)",
        value: "2097152",
        commented: false,
        overrides: "LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB",
        description: "Soft restart ceiling applied when a call needs a different import set than the live worker holds. Must not exceed the post-job ceiling. Default 2 GiB.",
    },
    FieldDoc {
        key: "runtime.module_cache_rss_guard_kib",
        ty: "integer (KiB)",
        value: "2097152",
        commented: false,
        overrides: "LEAN_HOST_MCP_MODULE_CACHE_RSS_GUARD_KIB",
        description: "Resident-memory ceiling above which the per-worker module-query cache stops growing. Default 2 GiB.",
    },
    FieldDoc {
        key: "runtime.module_cache_max_bytes",
        ty: "integer (bytes)",
        value: "33554432",
        commented: false,
        overrides: "LEAN_HOST_MCP_MODULE_CACHE_MAX_BYTES",
        description: "Maximum size of the per-worker module-query result cache, in bytes. Default 32 MiB.",
    },
    FieldDoc {
        key: "runtime.request_timeout_millis",
        ty: "integer (ms)",
        value: "120000",
        commented: false,
        overrides: "LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS",
        description: "Per-request worker deadline covering one tool call end to end. On expiry the worker is recycled and the call returns a retryable runtime error. Raise it for unusually heavy modules whose verify/proof_state legitimately runs longer; lower it to bound whole-project scans (e.g. find_references at project scope). Default 120 s.",
    },
    FieldDoc {
        key: "runtime.project_mailbox_capacity",
        ty: "integer",
        value: "8",
        commented: false,
        overrides: "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
        description: "How many calls may queue for one project's worker before new calls are shed with a retryable busy status.",
    },
    FieldDoc {
        key: "runtime.worker_restart_limit",
        ty: "integer",
        value: "3",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_RESTART_LIMIT",
        description: "How many worker restarts are tolerated within the restart window before the project is marked unhealthy.",
    },
    FieldDoc {
        key: "runtime.worker_restart_window_secs",
        ty: "integer (s)",
        value: "60",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_RESTART_WINDOW_SECS",
        description: "Rolling window, in seconds, over which worker_restart_limit is counted.",
    },
    // ---- [broker] — project pool + semantic admission -------------------
    FieldDoc {
        key: "broker.max_projects",
        ty: "integer",
        value: "4",
        commented: false,
        overrides: "LEAN_HOST_MCP_MAX_PROJECTS",
        description: "How many distinct Lake projects stay open at once; on overflow the least-recently-used project's worker is evicted.",
    },
    FieldDoc {
        key: "broker.idle_timeout_secs",
        ty: "integer (s)",
        value: "600",
        commented: false,
        overrides: "LEAN_HOST_MCP_IDLE_TIMEOUT_SECS",
        description: "Evict a project's worker after this many idle seconds. 0 disables idle eviction. Default 10 minutes.",
    },
    FieldDoc {
        key: "broker.semantic_permits",
        ty: "integer",
        value: "1",
        commented: false,
        overrides: "LEAN_HOST_MCP_SEMANTIC_PERMITS",
        description: "How many semantic (elaborating) calls run concurrently across all projects. Lean elaboration is single-threaded per worker, so raising this helps only when hosting several projects at once.",
    },
    FieldDoc {
        key: "broker.semantic_waiters",
        ty: "integer",
        value: "16",
        commented: false,
        overrides: "LEAN_HOST_MCP_SEMANTIC_WAITERS",
        description: "How many semantic calls may queue for a permit before new ones are shed with a retryable semantic_admission_full status.",
    },
    FieldDoc {
        key: "broker.semantic_admission_timeout_millis",
        ty: "integer (ms)",
        value: "60000",
        commented: false,
        overrides: "LEAN_HOST_MCP_SEMANTIC_ADMISSION_TIMEOUT_MILLIS",
        description: "How long a semantic call waits for a permit before giving up with a retryable semantic_admission_timeout status. Default 60 seconds.",
    },
    // ---- [server] — transport (CLI/env still override) ------------------
    FieldDoc {
        key: "server.bind",
        ty: "string (loopback ADDR:PORT)",
        value: "\"127.0.0.1:8765\"",
        commented: true,
        overrides: "--bind / LEAN_HOST_MCP_BIND",
        description: "Loopback address for the Streamable HTTP transport; omit for stdio (the default). Non-loopback addresses are rejected: the server has no built-in authentication or TLS.",
    },
    FieldDoc {
        key: "server.http_path",
        ty: "string",
        value: "\"/mcp\"",
        commented: true,
        overrides: "--http-path / LEAN_HOST_MCP_HTTP_PATH",
        description: "HTTP route for the Streamable HTTP transport. Requires bind. Default /mcp.",
    },
    FieldDoc {
        key: "server.response_carrier",
        ty: "string (text, structured, both)",
        value: "\"text\"",
        commented: false,
        overrides: "LEAN_HOST_MCP_RESPONSE_CARRIER",
        description: "Which field of the tool result carries the envelope. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text.",
    },
    // ---- [telemetry] — model-facing envelope verbosity ------------------
    FieldDoc {
        key: "telemetry.verbosity",
        ty: "string (quiet, full)",
        value: "\"quiet\"",
        commented: false,
        overrides: "LEAN_HOST_MCP_TELEMETRY_VERBOSITY",
        description: "How much operational telemetry the envelope carries. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet.",
    },
    // ---- [output] — per-call output budget overrides --------------------
    FieldDoc {
        key: "output.max_field_bytes",
        ty: "integer (bytes)",
        value: "8192",
        commented: true,
        overrides: "LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES",
        description: "Override the per-field output byte cap for all tools. Unset keeps each tool's built-in default (8 KiB for inspection, 4 KiB for proof actions). Clamped to 256 bytes to 64 KiB.",
    },
    FieldDoc {
        key: "output.max_total_bytes",
        ty: "integer (bytes)",
        value: "65536",
        commented: true,
        overrides: "LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES",
        description: "Override the total output byte cap for all tools. Unset keeps the built-in 64 KiB default. Clamped to 1 KiB to 64 KiB.",
    },
    FieldDoc {
        key: "output.heartbeat_limit",
        ty: "integer (heartbeats)",
        value: "200000",
        commented: true,
        overrides: "LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT",
        description: "Default elaboration heartbeat budget for try_proof_step and verify_declaration. Unset uses the worker default. Bounds runaway tactics.",
    },
];

/// Split a dotted key into `(table, leaf)`. A top-level key (no `.`) returns an
/// empty table.
fn split_key(key: &str) -> (&str, &str) {
    key.rsplit_once('.').unwrap_or(("", key))
}

/// Render the documented starter config `config init` writes.
///
/// Every knob with a built-in default is emitted active at that default;
/// optional knobs with no default (`primary_project`, `server.bind`,
/// `server.http_path`) are emitted commented out, so a freshly-generated file
/// reproduces the current defaults and changes nothing until edited.
#[must_use]
pub fn render_default_toml() -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(
        "# lean-host-mcp configuration. Generated by `lean-host-mcp config init`.\n\
         #\n\
         # Place this file at `lean-host-mcp.toml` in (or above) the directory you\n\
         # launch the server from, or at `~/.config/lean-host-mcp/config.toml` for a\n\
         # per-user default. When both exist they merge per key, the local file\n\
         # winning. Precedence for every knob is: CLI flag > env var > this file >\n\
         # built-in default, so an env var still overrides what you set here.\n\
         #\n\
         # Active lines below are set to the current built-in defaults. Commented\n\
         # lines are optional knobs with no default; uncomment to opt in.\n",
    );

    // A sentinel that no real table name equals, so the first field always
    // opens its group.
    let mut current = "\0";
    for field in SCHEMA_FIELDS {
        let (table, leaf) = split_key(field.key);
        if table == current {
            out.push('\n');
        } else {
            current = table;
            out.push('\n');
            if !table.is_empty() {
                out.push('[');
                out.push_str(table);
                out.push_str("]\n");
            }
        }
        out.push_str("# ");
        out.push_str(field.description);
        out.push_str("\n# Type: ");
        out.push_str(field.ty);
        out.push_str(". Override: ");
        out.push_str(field.overrides);
        out.push_str(".\n");
        if field.commented {
            out.push_str("# ");
        }
        out.push_str(leaf);
        out.push_str(" = ");
        out.push_str(field.value);
        out.push('\n');
    }
    out
}

/// Render the Markdown reference table embedded in `docs/operations.md`
/// between its `BEGIN GENERATED` / `END GENERATED` markers. Columns: key, type,
/// default, override, description.
#[must_use]
pub fn render_reference_table() -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("| Key | Type | Default | Override | Description |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    for field in SCHEMA_FIELDS {
        let default = if field.commented {
            "unset".to_owned()
        } else {
            format!("`{}`", field.value)
        };
        out.push_str("| `");
        out.push_str(field.key);
        out.push_str("` | ");
        out.push_str(field.ty);
        out.push_str(" | ");
        out.push_str(&default);
        out.push_str(" | `");
        out.push_str(field.overrides);
        out.push_str("` | ");
        out.push_str(field.description);
        out.push_str(" |\n");
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert the branch under test directly"
)]
mod tests {
    use super::*;
    use crate::broker;
    use crate::config_file::ConfigFile;
    use crate::project::ProjectRuntimeConfig;

    /// No description or type may contain `|`, which would corrupt the
    /// Markdown table without escaping.
    #[test]
    fn no_field_text_contains_a_pipe() {
        for field in SCHEMA_FIELDS {
            assert!(!field.description.contains('|'), "pipe in {}", field.key);
            assert!(!field.ty.contains('|'), "pipe in {} type", field.key);
            assert!(!field.overrides.contains('|'), "pipe in {} override", field.key);
        }
    }

    /// The generated starter file must be valid TOML that deserializes into the
    /// real schema. Commented optional knobs contribute nothing.
    #[test]
    fn generated_toml_parses_into_config_schema() {
        let toml = render_default_toml();
        let config: ConfigFile =
            toml::from_str(&toml).unwrap_or_else(|e| panic!("generated TOML is invalid: {e}\n{toml}"));
        // The optional, commented knobs stay unset.
        assert!(config.primary_project.is_none());
        assert!(config.server.bind.is_none());
        assert!(config.server.http_path.is_none());
    }

    /// Each active default in the catalogue must equal the built-in constant it
    /// documents. This is the guard against the file and the code drifting: bump
    /// a default in `project.rs`/`broker.rs` and forget the catalogue, and this
    /// fails.
    #[test]
    fn generated_defaults_match_builtin_constants() {
        let config: ConfigFile = toml::from_str(&render_default_toml()).unwrap();
        let rt = ProjectRuntimeConfig::default();

        assert_eq!(
            config.runtime.worker_rss_post_job_restart_kib,
            Some(rt.worker_rss_post_job_restart_kib())
        );
        assert_eq!(
            config.runtime.worker_rss_hard_kill_kib,
            Some(rt.worker_rss_hard_kill_kib())
        );
        assert_eq!(
            config.runtime.worker_rss_sample_millis,
            Some(rt.worker_rss_sample_millis())
        );
        assert_eq!(
            config.runtime.import_switch_rss_soft_kib,
            Some(rt.import_switch_rss_soft_kib())
        );
        assert_eq!(
            config.runtime.module_cache_rss_guard_kib,
            Some(rt.module_cache_rss_guard_kib())
        );
        assert_eq!(config.runtime.module_cache_max_bytes, Some(rt.module_cache_max_bytes()));
        assert_eq!(config.runtime.request_timeout_millis, Some(rt.request_timeout_millis()));
        assert_eq!(config.runtime.project_mailbox_capacity, Some(rt.mailbox_capacity()));
        assert_eq!(config.runtime.worker_restart_limit, Some(rt.max_restarts_per_window()));
        assert_eq!(
            config.runtime.worker_restart_window_secs,
            Some(rt.restart_window().as_secs())
        );

        assert_eq!(config.broker.max_projects, Some(broker::DEFAULT_MAX_PROJECTS));
        assert_eq!(config.broker.idle_timeout_secs, Some(broker::DEFAULT_IDLE_TIMEOUT_SECS));
        assert_eq!(config.broker.semantic_permits, Some(broker::DEFAULT_SEMANTIC_PERMITS));
        assert_eq!(config.broker.semantic_waiters, Some(broker::DEFAULT_SEMANTIC_WAITERS));
        assert_eq!(
            config.broker.semantic_admission_timeout_millis,
            Some(broker::DEFAULT_SEMANTIC_ADMISSION_TIMEOUT_MILLIS)
        );
    }

    /// Reduce a Markdown table to a grid of trimmed cells, normalising the
    /// header-separator dashes, so the parity check is robust to the Markdown
    /// formatter re-padding columns.
    fn table_cells(table: &str) -> Vec<Vec<String>> {
        table
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('|'))
            .map(|line| {
                line.trim_matches('|')
                    .split('|')
                    .map(|cell| {
                        let cell = cell.trim();
                        if !cell.is_empty() && cell.chars().all(|c| c == '-') {
                            "---".to_owned()
                        } else {
                            cell.to_owned()
                        }
                    })
                    .collect()
            })
            .collect()
    }

    /// The reference table committed in `docs/operations.md` must match what
    /// this module renders. Regenerate the block when this fails.
    #[test]
    fn operations_md_reference_table_is_in_sync() {
        const DOC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/operations.md"));
        let begin = DOC
            .find("BEGIN GENERATED")
            .expect("BEGIN GENERATED marker in operations.md");
        let after_begin = DOC[begin..]
            .find("-->")
            .map(|i| begin + i + 3)
            .expect("end of BEGIN marker");
        let end = DOC[after_begin..]
            .find("END GENERATED")
            .map(|i| after_begin + i)
            .expect("END GENERATED marker in operations.md");
        // Trim back off the trailing `<!--` that opens the END marker comment.
        let block = DOC[after_begin..end].trim_end().trim_end_matches("<!--").trim();
        assert_eq!(
            table_cells(block),
            table_cells(&render_reference_table()),
            "docs/operations.md reference table is stale; regenerate it from config_schema::render_reference_table()"
        );
    }
}
