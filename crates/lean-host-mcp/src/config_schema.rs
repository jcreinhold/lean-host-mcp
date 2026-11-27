//! The one catalogue of every configuration knob.
//!
//! A knob is defined once, here, as a `FieldDoc` row in `SCHEMA_FIELDS`:
//! its TOML key, type, default, the env var / CLI flag that overrides it, and
//! a one-line description. Two renderers consume that catalogue —
//! [`render_default_toml`] writes the documented starter file `config init`
//! produces, and the test-only `render_reference_table` writes the Markdown
//! table embedded in `docs/operations.md`. Neither output is hand-maintained, so the file, the
//! docs, and the code can't drift from one another. A test ties each row's
//! default to the built-in constant it documents, and another asserts the
//! committed docs table matches what this module renders.
//!
//! The schema *structs* and the file loading live next door in
//! [`crate::config_file`]; this module is their human-facing companion.

/// One configuration knob: everything a reader or a generator needs to know
/// about it, in one place.
struct FieldDoc {
    /// Dotted TOML path, e.g. `"runtime.request_timeout_millis"`.
    key: &'static str,
    /// Human-readable type and unit, e.g. `"integer (KiB)"`.
    ty: &'static str,
    /// The value shown in the generated file, as a TOML literal (string
    /// literals include their quotes). For a knob with a built-in default this
    /// is that default; for an optional knob it is an illustrative example.
    value: &'static str,
    /// Whether the generated file comments the line out. `true` for optional
    /// knobs that have no built-in literal default (their absence selects a
    /// behaviour, such as stdio or a path under the Lake root), so a fresh file
    /// changes nothing until the user opts in.
    commented: bool,
    /// The env var, and CLI flag where one exists, that override this knob;
    /// empty when nothing does. Precedence is CLI flag > env var > local file >
    /// tracked file > built-in default.
    overrides: &'static str,
    /// One-line description. Single line so it serves both the file comment and
    /// the Markdown table cell; must not contain `|`.
    description: &'static str,
}

/// Every knob, in the order it appears in the generated file and docs table:
/// `[runtime]`, `[broker]`, `[server]`, `[telemetry]`, `[output]`, `[install]`,
/// `[requests]`, `[build]`, `[recipes.<name>]`.
const SCHEMA_FIELDS: &[FieldDoc] = &[
    // ---- [runtime] — worker memory + lifecycle --------------------------
    FieldDoc {
        key: "runtime.lean_max_memory_kib",
        ty: "integer (KiB)",
        value: "18874368",
        commented: true,
        overrides: "LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB",
        description: "Lean heap ceiling per worker child. Allocator exhaustion can abort the child when a C++ exception reaches the Rust FFI boundary. Unset derives the residue budget plus max(residue budget, 8 GiB), converted to KiB, leaving headroom above clean import recycling. Startup warns when an explicit ceiling is at or below the residue budget.",
    },
    FieldDoc {
        key: "runtime.request_timeout_millis",
        ty: "integer (ms)",
        value: "120000",
        commented: false,
        overrides: "LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS",
        description: "One shared budget for worker-capacity admission, mailbox admission and queued waiting for each worker operation. After dispatch, worker execution has a separate timeout of the same length; execution expiry recycles the worker. Admission expiry does not dispatch that queued operation. This is not a whole-tool deadline: preparation and multiple operations can take longer. Default 120 s.",
    },
    FieldDoc {
        key: "runtime.project_mailbox_capacity",
        ty: "integer",
        value: "16",
        commented: false,
        overrides: "LEAN_HOST_MCP_PROJECT_MAILBOX_CAPACITY",
        description: "Maximum queued calls in each worker actor's mailbox. Further callers wait in FIFO order for mailbox space within the same original request_timeout_millis admission deadline. Cancellation removes only that caller.",
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
    FieldDoc {
        key: "runtime.worker_import_residue_budget_mib",
        ty: "integer (MiB)",
        value: "9216",
        commented: true,
        overrides: "LEAN_HOST_MCP_WORKER_IMPORT_RESIDUE_BUDGET_MIB",
        description: "Unreclaimable import bytes per worker before a clean recycle. Unset uses a quarter of RAM divided among broker.max_workers, clamped to 9-12 GiB and capped at each worker's RAM share minus 4608 MiB (minimum 1 MiB). This affordability cap can lower the default below 9 GiB. Explicit MiB values bypass RAM sizing; a budget below one import's residue can force frequent recycling.",
    },
    FieldDoc {
        key: "runtime.worker_session_pool_capacity",
        ty: "integer",
        value: "8",
        commented: false,
        overrides: "LEAN_HOST_MCP_WORKER_SESSION_POOL_CAPACITY",
        description: "How many imported Lean environments one worker child keeps warm. Returning to a pooled import profile costs a key comparison instead of a full import, so this should cover the number of distinct per-file import profiles a session moves between — a changed-file sweep touches one per file. A held environment costs about 70 MiB against 2-4 GB for the import it saves. Default 8.",
    },
    // ---- [broker] — worker pool ----------------------------------------
    FieldDoc {
        key: "broker.max_workers",
        ty: "integer",
        value: "4",
        commented: false,
        overrides: "LEAN_HOST_MCP_MAX_WORKERS",
        description: "Worker processes the server may run at once, counting workers that are starting or still exiting. An exiting worker keeps its slot until its exit is confirmed, so a value above max_workers_per_project lets a new worker start while an old one exits. Default 4.",
    },
    FieldDoc {
        key: "broker.max_workers_per_project",
        ty: "integer",
        value: "1",
        commented: false,
        overrides: "LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT",
        description: "Workers that may serve the project at once. An extra worker opens only when a slot is free; otherwise requests queue on an existing worker. Set 2 only after measuring the memory cost of concurrent checks: each worker keeps its own imports and cache. Default 1.",
    },
    FieldDoc {
        key: "broker.idle_timeout_secs",
        ty: "integer (s)",
        value: "600",
        commented: false,
        overrides: "LEAN_HOST_MCP_IDLE_TIMEOUT_SECS",
        description: "Stop a worker after this many idle seconds. 0 turns idle retirement off. Default 10 minutes.",
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
        description: "Which field of the tool result carries the semantic response. text emits one content text block (what the model reads); structured emits only structuredContent; both duplicates onto both. Default text.",
    },
    // ---- [telemetry] — model-facing envelope verbosity ------------------
    FieldDoc {
        key: "telemetry.verbosity",
        ty: "string (quiet, full)",
        value: "\"quiet\"",
        commented: false,
        overrides: "LEAN_HOST_MCP_TELEMETRY_VERBOSITY",
        description: "How much operational telemetry the internal operation envelope keeps before semantic response adaptation. quiet keeps proof-relevant content and drops the runtime block, manifest hash, and full import list; full emits everything for debugging. Default quiet.",
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
        description: "Default elaboration heartbeat budget for lean_trial proof_step and lean_verify target groups. Unset uses the worker default. Bounds runaway tactics.",
    },
    FieldDoc {
        key: "output.slow_call_warning_millis",
        ty: "integer (ms)",
        value: "10000",
        commented: false,
        overrides: "LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS",
        description: "Worker-call CPU time above which every tool response carries a warning naming the elapsed milliseconds and recommending refactoring (splitting the declaration, explicit type annotations, extracted lemmas). Keys on call_cpu_millis, not wallclock, so machine contention never triggers it. Default 10 s.",
    },
    // ---- [install] — paths `lean-host-mcp install` records ---------------
    FieldDoc {
        key: "install.binary",
        ty: "path",
        value: "\"/abs/path/to/project/.local/lean-host-mcp/bin/lean-host-mcp\"",
        commented: true,
        overrides: "",
        description: "The server executable that install copied, for MCP client configs and scripts that run the server by absolute path. The server itself ignores it.",
    },
    FieldDoc {
        key: "install.workers_dir",
        ty: "path",
        value: "\"/abs/path/to/project/.local/lean-host-mcp/workers\"",
        commented: true,
        overrides: "LEAN_HOST_MCP_WORKERS_DIR",
        description: "Directory of per-toolchain worker installs, shared by install, install-worker and the server. Unset uses .local/lean-host-mcp/workers under the Lake root. A relative path is relative to the Lake root.",
    },
    FieldDoc {
        key: "install.store_dir",
        ty: "path",
        value: "\"/abs/path/to/project/.local/lean-host-mcp/store\"",
        commented: true,
        overrides: "LEAN_HOST_MCP_STORE_DIR",
        description: "Private server state directory (mode 0700). Its lock admits one server at a time; a second server on the same store exits and names the first. It also holds the native owner records (owners/), the semantic runtime cache (semantic-runtimes/) and the server process records (processes/). Unset uses .local/lean-host-mcp/store under the Lake root. A relative path is relative to the Lake root.",
    },
    FieldDoc {
        key: "install.elan_home",
        ty: "path",
        value: "\"/abs/path/to/home/.elan\"",
        commented: true,
        overrides: "ELAN_HOME",
        description: "Elan's home when install ran: ELAN_HOME, else $HOME/.elan. When ELAN_HOME is unset, the server and install-worker --lake-root look for the Lean toolchain here, not under $HOME, and pass it on as ELAN_HOME.",
    },
    // ---- [requests] — the request table ----------------------------------
    FieldDoc {
        key: "requests.retention_hours",
        ty: "integer (h)",
        value: "168",
        commented: false,
        overrides: "",
        description: "Hours an ended request keeps its row, its stored result and its build log. The server prunes at start and every hour; it never prunes a request that has not ended. Default 7 days.",
    },
    FieldDoc {
        key: "requests.max_rows",
        ty: "integer",
        value: "10000",
        commented: false,
        overrides: "",
        description: "Most rows the request table keeps. Past this, pruning deletes the ended rows that finished first. Default 10000.",
    },
    FieldDoc {
        key: "requests.wait_max_secs",
        ty: "integer (s)",
        value: "300",
        commented: false,
        overrides: "",
        description: "Longest wait_seconds one lean_get may ask for. Default 300 s.",
    },
    // ---- [build] — lean_build -------------------------------------------
    FieldDoc {
        key: "build.guard",
        ty: "array of Lake targets",
        value: "[]",
        commented: false,
        overrides: "",
        description: "Targets that must already be built. A guard step runs lake build --no-build on them, which fails without building anything when one is out of date. It runs before a build of targets; a recipe's guard key places it. A project on Mathlib can use [\"@mathlib/+Mathlib:olean\"]. Empty runs no guard.",
    },
    FieldDoc {
        key: "build.timeout_secs",
        ty: "integer (s)",
        value: "14400",
        commented: false,
        overrides: "",
        description: "Deadline for one build, counted from its request: the wait for the project's write turn and every step count. Default 4 hours.",
    },
    FieldDoc {
        key: "build.log_limit_mib",
        ty: "integer (MiB)",
        value: "16",
        commented: false,
        overrides: "",
        description: "Cap on one build's log, which lives in the store's logs directory. Output past the cap is dropped, and the reply says so. Default 16 MiB.",
    },
    // ---- [recipes.<name>] — Lake commands a client runs by name ----------
    FieldDoc {
        key: "recipes.<name>.steps",
        ty: "array of arrays of strings",
        value: "[[\"build\", \"MyProject\"], [\"lint\"]]",
        commented: true,
        overrides: "",
        description: "The Lake runs of one recipe, in order. Each is the words after lake and may not be empty; Lake reports its own errors in them. The first run that fails ends the recipe. Steps run in the Lake root with the server's environment. lean_build runs a recipe by its name.",
    },
    FieldDoc {
        key: "recipes.<name>.append",
        ty: "string (none, last, each)",
        value: "\"none\"",
        commented: false,
        overrides: "",
        description: "Which steps get the request's args at their end: none refuses args, last gives them to the last step, each to every step. An arg may not be empty or start with -. Default none.",
    },
    FieldDoc {
        key: "recipes.<name>.guard",
        ty: "string (before, after, none)",
        value: "\"before\"",
        commented: false,
        overrides: "",
        description: "Where the build.guard step runs: before the steps, after them, or not at all. A failing guard before the steps ends the recipe. Default before.",
    },
    FieldDoc {
        key: "recipes.<name>.timeout_secs",
        ty: "integer (s)",
        value: "3600",
        commented: true,
        overrides: "",
        description: "Deadline for one build of this recipe, in place of build.timeout_secs. Unset uses build.timeout_secs.",
    },
];

/// Split a dotted key into `(table, leaf)`. A top-level key (no `.`) returns an
/// empty table.
fn split_key(key: &str) -> (&str, &str) {
    key.rsplit_once('.').unwrap_or(("", key))
}

/// Whether `table` is a real table name rather than a pattern such as
/// `recipes.<name>`, which the generated file must comment out.
fn is_bare_path(table: &str) -> bool {
    table
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Whether some row documents a key of table `name`.
pub(crate) fn is_section(name: &str) -> bool {
    SCHEMA_FIELDS.iter().any(|field| split_key(field.key).0 == name)
}

/// Whether the server reads the dotted key `key`.
pub(crate) fn is_documented(key: &str) -> bool {
    SCHEMA_FIELDS.iter().any(|field| field.key == key)
}

/// Render the documented starter config `config init` writes.
///
/// Every knob with a built-in default is emitted active at that default;
/// optional knobs with no literal default (`server.bind`, `server.http_path`,
/// the `[install]` paths) are emitted commented out, so a freshly-generated
/// file reproduces the current defaults and changes nothing until edited. A
/// pattern table such as `[recipes.<name>]` is commented out whole.
#[must_use]
pub(crate) fn render_default_toml() -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(
        "# lean-host-mcp configuration. Generated by `lean-host-mcp config init`.\n\
         #\n\
         # The server reads `lean-host-mcp.toml` at its Lake root (`--lake-root`),\n\
         # then `.local/lean-host-mcp/config.toml` under the same root, which\n\
         # `lean-host-mcp install` writes. The two merge per key and the local\n\
         # file wins. Precedence for every knob is: CLI flag > env var > local\n\
         # file > tracked file > built-in default, so an env var still overrides\n\
         # what you set here.\n\
         #\n\
         # Active lines below are set to the current built-in defaults. Commented\n\
         # lines are optional knobs with no default; uncomment to opt in. The\n\
         # [recipes.<name>] lines show one recipe: copy them under a header\n\
         # such as [recipes.lint] for each recipe.\n",
    );

    // A sentinel that no real table name equals, so the first field always
    // opens its group.
    let mut current = "\0";
    for field in SCHEMA_FIELDS {
        let (table, leaf) = split_key(field.key);
        let pattern = !is_bare_path(table);
        if table == current {
            out.push('\n');
        } else {
            current = table;
            out.push('\n');
            if !table.is_empty() {
                if pattern {
                    out.push_str("# ");
                }
                out.push('[');
                out.push_str(table);
                out.push_str("]\n");
            }
        }
        out.push_str("# ");
        out.push_str(field.description);
        out.push_str("\n# Type: ");
        out.push_str(field.ty);
        if !field.overrides.is_empty() {
            out.push_str(". Override: ");
            out.push_str(field.overrides);
        }
        out.push_str(".\n");
        if field.commented || pattern {
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
#[cfg(test)]
fn render_reference_table() -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("| Key | Type | Default | Override | Description |\n");
    out.push_str("| --- | --- | --- | --- | --- |\n");
    for field in SCHEMA_FIELDS {
        let default = if field.commented {
            "unset".to_owned()
        } else {
            format!("`{}`", field.value)
        };
        let overrides = if field.overrides.is_empty() {
            "none".to_owned()
        } else {
            format!("`{}`", field.overrides)
        };
        out.push_str("| `");
        out.push_str(field.key);
        out.push_str("` | ");
        out.push_str(field.ty);
        out.push_str(" | ");
        out.push_str(&default);
        out.push_str(" | ");
        out.push_str(&overrides);
        out.push_str(" | ");
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
        assert!(config.server.bind.is_none());
        assert!(config.server.http_path.is_none());
        assert!(config.install.binary.is_none());
        assert!(config.install.workers_dir.is_none());
        assert!(config.install.store_dir.is_none());
        assert!(config.install.elan_home.is_none());
    }

    /// Each active default in the catalogue must equal the built-in constant it
    /// documents. This is the guard against the file and the code drifting: bump
    /// a default in `project.rs`/`broker.rs` and forget the catalogue, and this
    /// fails.
    #[test]
    fn generated_defaults_match_builtin_constants() {
        let config: ConfigFile = toml::from_str(&render_default_toml()).unwrap();
        let rt = ProjectRuntimeConfig::default();

        // Commented out for the same reason as the residue budget below: it is
        // derived from that budget, so it inherits the RAM dependence. What the
        // catalogue can pin is the ordering the derivation exists to guarantee.
        assert_eq!(config.runtime.lean_max_memory_kib, None);
        assert!(
            rt.lean_max_memory_kib().saturating_mul(1024) > rt.import_residue_budget_bytes(),
            "the heap ceiling must stay above the residue budget, or the abort preempts the clean recycle"
        );
        assert_eq!(config.runtime.request_timeout_millis, Some(rt.request_timeout_millis()));
        assert_eq!(config.runtime.project_mailbox_capacity, Some(rt.mailbox_capacity()));
        assert_eq!(config.runtime.worker_restart_limit, Some(rt.max_restarts_per_window()));
        assert_eq!(
            config.runtime.worker_restart_window_secs,
            Some(rt.restart_window().as_secs())
        );
        // Commented out in the generated file on purpose: its default is derived
        // from system RAM and the pool size, so no literal in this catalogue
        // could match it on every machine. Absence is what "derive it" means;
        // the derived value is asserted to stay at or below the documented
        // ceiling (the floor gives way to one child's share of the machine) and
        // to be the derivation for the catalogue's own default pool size.
        assert_eq!(config.runtime.worker_import_residue_budget_mib, None);
        let derived_mib = rt.import_residue_budget_bytes() / (1024 * 1024);
        assert!(
            (1..=12288).contains(&derived_mib),
            "derived residue budget {derived_mib} MiB escaped the bounds this catalogue documents"
        );
        assert_eq!(
            rt.import_residue_budget_bytes(),
            ProjectRuntimeConfig::default_for_workers(broker::DEFAULT_MAX_WORKERS).import_residue_budget_bytes()
        );
        assert_eq!(
            config.runtime.worker_session_pool_capacity,
            Some(rt.session_pool_capacity())
        );

        assert_eq!(config.broker.max_workers, Some(broker::DEFAULT_MAX_WORKERS));
        assert_eq!(config.broker.max_workers_per_project, Some(1));
        assert_eq!(config.broker.idle_timeout_secs, Some(broker::DEFAULT_IDLE_TIMEOUT_SECS));
        assert_eq!(
            config.output.slow_call_warning_millis,
            Some(crate::tools::DEFAULT_SLOW_CALL_WARNING_MILLIS)
        );
        assert_eq!(
            config.requests.retention_hours,
            Some(crate::requests::DEFAULT_RETENTION_HOURS)
        );
        assert_eq!(config.requests.max_rows, Some(crate::requests::DEFAULT_MAX_ROWS));
        assert_eq!(
            config.requests.wait_max_secs,
            Some(crate::requests::DEFAULT_WAIT_MAX_SECS)
        );
    }

    /// The generated `[build]` lines hold the built-in build defaults, and the
    /// commented recipe pattern adds no recipe.
    #[test]
    fn generated_build_tables_are_the_defaults() {
        use crate::build::config::BuildConfig;
        let text = render_default_toml();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert!(text.contains("\n# [recipes.<name>]\n"));
        assert!(!table.contains_key("recipes"));
        assert_eq!(
            BuildConfig::from_table(&table).unwrap(),
            BuildConfig::from_table(&toml::Table::new()).unwrap()
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
