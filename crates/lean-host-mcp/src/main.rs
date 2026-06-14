//! `lean-host-mcp`—Model Context Protocol server hosting Lean 4 via a
//! supervised [`lean-rs-worker`] child.
//!
//! Stdio is the default transport. `--bind` selects Streamable HTTP. With a
//! Lake project visible from the invocation directory (or one set via
//! `LEAN_HOST_MCP_PROJECT` / `~/.config/lean-host-mcp/config.toml`), no
//! project flags are required.

mod transport_http;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

use lean_host_mcp::cli::config_init::{self, ConfigCommand};
use lean_host_mcp::cli::install_worker::{self, InstallWorkerArgs};
use lean_host_mcp::cli::processes::{self, DoctorProcessesArgs};
use lean_host_mcp::{
    BrokerConfig, ConfigFile, LeanHostService, OutputBudgetOverrides, ProjectBroker, ProjectRuntimeConfig,
    ResponseCarrier, TelemetryVerbosity, ToolConfig,
};

/// Stdio MCP server that hosts a Lean 4 environment via a worker child.
#[derive(Debug, Parser)]
#[command(name = "lean-host-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the stdio MCP server (default when no subcommand is given).
    Serve(ServeArgs),
    /// Build and install a per-toolchain worker binary.
    InstallWorker(InstallWorkerArgs),
    /// Generate and manage the configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect host-written server process records.
    Doctor {
        #[command(subcommand)]
        command: DoctorCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    /// List registered lean-host-mcp server PIDs and remove stale records.
    Processes(DoctorProcessesArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Default Lake project for tool calls that don't specify one.
    /// Equivalent to `LEAN_HOST_MCP_PROJECT`. May be omitted; the server
    /// will resolve from the invocation cwd's nearest lakefile, then from
    /// `~/.config/lean-host-mcp/config.toml`. Per-call `project="..."`
    /// arguments always win.
    #[arg(long, env = "LEAN_HOST_MCP_PROJECT")]
    lake_root: Option<PathBuf>,

    /// Loopback address for Streamable HTTP, e.g. 127.0.0.1:8765.
    /// If omitted, the server uses stdio.
    #[arg(long, env = "LEAN_HOST_MCP_BIND")]
    bind: Option<SocketAddr>,

    /// HTTP route for Streamable HTTP. Requires --bind.
    #[arg(long, env = "LEAN_HOST_MCP_HTTP_PATH")]
    http_path: Option<String>,
}

const DEFAULT_HTTP_PATH: &str = "/mcp";

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Some(Command::InstallWorker(args)) => match install_worker::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("lean-host-mcp install-worker: {err}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Config { command }) => match config_init::run(&command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("lean-host-mcp config: {err}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Doctor {
            command: DoctorCommand::Processes(args),
        }) => match processes::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("lean-host-mcp doctor processes: {err}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Serve(args)) => run_serve(args),
        None => run_serve(cli.serve),
    }
}

fn run_serve(args: ServeArgs) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("failed to build tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(serve(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "lean-host-mcp exited with error");
            ExitCode::FAILURE
        }
    }
}

#[allow(
    let_underscore_drop,
    reason = "try_init's failure means a subscriber is already installed; we'd continue without ours either way"
)]
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

#[allow(clippy::significant_drop_tightening)]
async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    // Merged config file (project-local `lean-host-mcp.toml` over the home
    // file). It is the layer beneath env/CLI: each knob is CLI > env > file >
    // default.
    let file = ConfigFile::load(&cwd);

    // Transport: clap already folds CLI-or-env into `args.*`, so `.or(file)`
    // yields CLI > env > file. The file's `bind` is a raw string, parsed here so
    // the library config schema stays transport-type-free.
    let file_bind = file
        .server
        .bind
        .as_deref()
        .map(|s| {
            s.parse::<SocketAddr>()
                .with_context(|| format!("config [server] bind is not a socket address: {s}"))
        })
        .transpose()?;
    let bind = args.bind.or(file_bind);
    let http_path = args.http_path.clone().or_else(|| file.server.http_path.clone());
    let http = http_config(bind, http_path.as_deref())?;

    let env_default = args.lake_root;
    let config_default = file.primary_project.clone();
    let (max_projects, idle_timeout, semantic_permits, semantic_waiters, semantic_admission_timeout, semantic_lock_dir) =
        BrokerConfig::pool_from_env_with_file(&file.broker)?;
    let runtime_config = ProjectRuntimeConfig::from_env_with_file(&file.runtime)?;
    let tool_config = resolve_tool_config(&file)?;
    let transport = if http.is_some() { "http" } else { "stdio" };
    let bind_display = http.as_ref().map(|config| config.bind.to_string());
    let http_path_log = http.as_ref().map(|config| config.path.as_str());

    tracing::info!(
        env_default = ?env_default,
        config_default = ?config_default,
        cwd = %cwd.display(),
        max_projects = %max_projects,
        idle_timeout_secs = idle_timeout.as_secs(),
        semantic_permits = %semantic_permits,
        semantic_waiters = %semantic_waiters,
        semantic_admission_timeout_millis = semantic_admission_timeout.as_millis(),
        semantic_lock_dir = %semantic_lock_dir.display(),
        worker_rss_post_job_restart_kib = runtime_config.worker_rss_post_job_restart_kib(),
        worker_rss_hard_kill_kib = runtime_config.worker_rss_hard_kill_kib(),
        worker_rss_sample_millis = runtime_config.worker_rss_sample_millis(),
        import_switch_rss_soft_kib = runtime_config.import_switch_rss_soft_kib(),
        project_mailbox_capacity = runtime_config.mailbox_capacity(),
        transport = transport,
        bind = bind_display.as_deref(),
        http_path = http_path_log,
        "starting lean-host-mcp",
    );

    let broker = build_broker(
        BrokerConfig {
            config_default,
            env_default,
            cwd,
            max_projects,
            idle_timeout,
            semantic_permits,
            semantic_waiters,
            semantic_admission_timeout,
            semantic_lock_dir,
        },
        runtime_config,
    );
    let _process_record = processes::ServerProcessRecord::register(transport, bind_display.clone(), http_path_log)?;
    let result = if let Some(config) = http {
        transport_http::serve(Arc::clone(&broker), config, tool_config).await
    } else {
        serve_stdio(Arc::clone(&broker), tool_config).await
    };
    broker.shutdown_all();
    result
}

// Stdio lifecycle model:
// - startup registers an exact-PID process record, then waits for MCP initialize
//   and tools/list through rmcp's stdio server.
// - initialize/tools-list failures are transport results; they unwind through
//   `server.waiting()` or `serve(stdio())`.
// - stdin EOF and client-side pipe loss resolve `server.waiting()`; dropping the
//   server lets broker shutdown drain/cancel resident project actors.
// - if the launching process disappears without a clean MCP shutdown, the
//   stdio parent watcher exits the server path; HTTP is intentionally separate
//   and runs until its signal/shutdown token fires.
// - broker shutdown clears resident projects; each project actor owns exactly
//   one worker child and invokes the upstream bounded worker shutdown.
// - doctor diagnostics only use registry PID, parent PID, process group, and
//   direct child metadata; cleanup removes stale records, never live processes.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the rmcp server handle must stay alive across the selected transport-wait and parent-loss futures"
)]
async fn serve_stdio(broker: Arc<ProjectBroker>, tool_config: ToolConfig) -> anyhow::Result<()> {
    let parent_pid = processes::current_parent_pid();
    let service = LeanHostService::new(broker, tool_config);
    let server = match service.serve(stdio()).await {
        Ok(server) => server,
        Err(err) => {
            let err = anyhow::Error::from(err);
            if stdio_connection_closed(&err) {
                tracing::info!(error = %err, "stdio transport closed before initialization completed");
                return Ok(());
            }
            tracing::warn!(error = %err, "stdio server failed before wait loop");
            return Err(err);
        }
    };

    tokio::select! {
        result = server.waiting() => {
            match &result {
                Ok(_) => tracing::info!("stdio transport closed"),
                Err(err) => tracing::warn!(error = %err, "stdio transport closed with error"),
            }
            match result {
                Ok(_) => Ok(()),
                Err(err) => {
                    let err = anyhow::Error::from(err);
                    if stdio_connection_closed(&err) {
                        Ok(())
                    } else {
                        Err(err)
                    }
                }
            }
        }
        () = wait_for_parent_loss(parent_pid) => {
            tracing::warn!(parent_pid = parent_pid, "stdio parent process disappeared");
            Ok(())
        }
    }
}

fn stdio_connection_closed(err: &anyhow::Error) -> bool {
    err.to_string().contains("connection closed")
}

async fn wait_for_parent_loss(parent_pid: Option<u32>) {
    let Some(parent_pid) = parent_pid.filter(|pid| *pid > 1) else {
        std::future::pending::<()>().await;
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        if !processes::process_alive(parent_pid) || processes::current_parent_pid() != Some(parent_pid) {
            return;
        }
    }
}

fn build_broker(config: BrokerConfig, runtime_config: ProjectRuntimeConfig) -> Arc<ProjectBroker> {
    ProjectBroker::new_with_runtime_config(config, runtime_config)
}

/// Resolve the presentation knobs (response carrier, telemetry verbosity, output
/// budgets) with the standard precedence: env var > config file > built-in
/// default. Invalid enum or integer values fail startup loudly rather than
/// silently falling back.
fn resolve_tool_config(file: &ConfigFile) -> anyhow::Result<ToolConfig> {
    let carrier =
        match tool_env_string("LEAN_HOST_MCP_RESPONSE_CARRIER").or_else(|| file.server.response_carrier.clone()) {
            Some(value) => ResponseCarrier::parse(&value)
                .with_context(|| format!("invalid response carrier {value:?}; expected text, structured, or both"))?,
            None => ResponseCarrier::default(),
        };
    let verbosity =
        match tool_env_string("LEAN_HOST_MCP_TELEMETRY_VERBOSITY").or_else(|| file.telemetry.verbosity.clone()) {
            Some(value) => TelemetryVerbosity::parse(&value)
                .with_context(|| format!("invalid telemetry verbosity {value:?}; expected quiet or full"))?,
            None => TelemetryVerbosity::default(),
        };
    let output = OutputBudgetOverrides {
        max_field_bytes: tool_env_int("LEAN_HOST_MCP_OUTPUT_MAX_FIELD_BYTES")?.or(file.output.max_field_bytes),
        max_total_bytes: tool_env_int("LEAN_HOST_MCP_OUTPUT_MAX_TOTAL_BYTES")?.or(file.output.max_total_bytes),
        heartbeat_limit: tool_env_int("LEAN_HOST_MCP_OUTPUT_HEARTBEAT_LIMIT")?.or(file.output.heartbeat_limit),
    };
    Ok(ToolConfig {
        carrier,
        verbosity,
        output,
    })
}

/// Read a non-empty environment variable as a trimmed `String`.
fn tool_env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Read an environment variable as an integer, failing startup on a malformed
/// value. Returns `None` when the variable is unset.
fn tool_env_int<T>(key: &str) -> anyhow::Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match tool_env_string(key) {
        Some(value) => value
            .parse::<T>()
            .map(Some)
            .map_err(|err| anyhow::anyhow!("invalid {key}={value:?}: {err}")),
        None => Ok(None),
    }
}

fn http_config(
    bind: Option<SocketAddr>,
    http_path: Option<&str>,
) -> anyhow::Result<Option<transport_http::HttpServeConfig>> {
    match (bind, http_path) {
        (None, None) => Ok(None),
        (None, Some(_)) => bail!("--http-path/LEAN_HOST_MCP_HTTP_PATH requires --bind/LEAN_HOST_MCP_BIND"),
        (Some(bind), path) => {
            validate_loopback_bind(bind)?;
            let path = path.unwrap_or(DEFAULT_HTTP_PATH);
            validate_http_path(path)?;
            Ok(Some(transport_http::HttpServeConfig {
                bind,
                path: path.to_owned(),
            }))
        }
    }
}

fn validate_loopback_bind(bind: SocketAddr) -> anyhow::Result<()> {
    if bind.ip().is_loopback() {
        Ok(())
    } else {
        bail!("--bind must be a loopback address; got {bind}")
    }
}

fn validate_http_path(path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        bail!("--http-path must not be empty");
    }
    if !path.starts_with('/') {
        bail!("--http-path must start with '/': {path}");
    }
    if path.contains('?') || path.contains('#') {
        bail!("--http-path must not contain a query string or fragment: {path}");
    }
    if path.contains('*') || path.contains('{') || path.contains('}') {
        bail!("--http-path must not contain route captures or wildcards: {path}");
    }
    path.parse::<axum::http::Uri>()
        .with_context(|| format!("--http-path is not a valid URI path: {path}"))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn no_bind_selects_stdio_even_with_lake_root() {
        let cli = Cli::parse_from(["lean-host-mcp", "--lake-root", "/tmp/project"]);
        assert!(cli.serve.bind.is_none());
        assert!(
            http_config(cli.serve.bind, cli.serve.http_path.as_deref())
                .expect("stdio config")
                .is_none()
        );
    }

    #[test]
    fn bind_selects_http_with_default_path() {
        let cli = Cli::parse_from(["lean-host-mcp", "--bind", "127.0.0.1:8765"]);
        let http = http_config(cli.serve.bind, cli.serve.http_path.as_deref())
            .expect("http config")
            .expect("http selected");
        assert_eq!(http.bind, "127.0.0.1:8765".parse().expect("socket addr"));
        assert_eq!(http.path, DEFAULT_HTTP_PATH);
    }

    #[test]
    fn http_path_without_bind_is_rejected() {
        let cli = Cli::parse_from(["lean-host-mcp", "--http-path", "/mcp"]);
        let err = http_config(cli.serve.bind, cli.serve.http_path.as_deref()).expect_err("http path requires bind");
        assert!(err.to_string().contains("requires --bind"));
    }

    #[test]
    fn non_loopback_bind_is_rejected() {
        let cli = Cli::parse_from(["lean-host-mcp", "--bind", "0.0.0.0:8765"]);
        let err = http_config(cli.serve.bind, cli.serve.http_path.as_deref()).expect_err("non-loopback should fail");
        assert!(err.to_string().contains("loopback"));
    }

    #[test]
    fn invalid_http_paths_are_rejected() {
        for path in ["", "mcp", "/mcp?x=1", "/mcp#frag", "/{*rest}", "/mcp/*rest"] {
            let err = validate_http_path(path).expect_err("invalid path should fail");
            assert!(
                err.to_string().contains("--http-path"),
                "unexpected error for {path:?}: {err}"
            );
        }
    }
}
