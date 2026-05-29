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

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

use lean_host_mcp::cli::install_worker::{self, InstallWorkerArgs};
use lean_host_mcp::{BrokerConfig, LeanHostService, ProjectBroker, ProjectRuntimeConfig};

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
    let http = http_config(&args)?;
    let env_default = args.lake_root;
    let config_default = read_config_default();
    let cwd = std::env::current_dir()?;
    let (max_projects, idle_timeout, semantic_permits, semantic_waiters, semantic_admission_timeout) =
        BrokerConfig::pool_from_env()?;
    let runtime_config = ProjectRuntimeConfig::from_env()?;
    let transport = if http.is_some() { "http" } else { "stdio" };
    let bind_display = http.as_ref().map(|config| config.bind.to_string());
    let http_path = http.as_ref().map(|config| config.path.as_str());

    tracing::info!(
        env_default = ?env_default,
        config_default = ?config_default,
        cwd = %cwd.display(),
        max_projects = %max_projects,
        idle_timeout_secs = idle_timeout.as_secs(),
        semantic_permits = %semantic_permits,
        semantic_waiters = %semantic_waiters,
        semantic_admission_timeout_millis = semantic_admission_timeout.as_millis(),
        worker_rss_post_job_restart_kib = runtime_config.worker_rss_post_job_restart_kib(),
        worker_rss_hard_kill_kib = runtime_config.worker_rss_hard_kill_kib(),
        worker_rss_sample_millis = runtime_config.worker_rss_sample_millis(),
        import_switch_rss_soft_kib = runtime_config.import_switch_rss_soft_kib(),
        project_mailbox_capacity = runtime_config.mailbox_capacity(),
        transport = transport,
        bind = bind_display.as_deref(),
        http_path = http_path,
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
        },
        runtime_config,
    );
    if let Some(config) = http {
        transport_http::serve(broker, config).await?;
    } else {
        let service = LeanHostService::new(broker);
        let server = service.serve(stdio()).await?;
        server.waiting().await?;
    }
    Ok(())
}

fn build_broker(config: BrokerConfig, runtime_config: ProjectRuntimeConfig) -> Arc<ProjectBroker> {
    ProjectBroker::new_with_runtime_config(config, runtime_config)
}

fn http_config(args: &ServeArgs) -> anyhow::Result<Option<transport_http::HttpServeConfig>> {
    match (args.bind, args.http_path.as_deref()) {
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

/// Top-level config schema. Reserved keys (e.g. future per-project
/// overrides) are accepted but ignored.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    primary_project: Option<PathBuf>,
}

/// Read `<config-dir>/lean-host-mcp/config.toml` and return its
/// `primary_project` if present. Missing file / missing key / parse
/// failures are all silent: the broker's resolution chain treats this as
/// "no default" and continues. The `LEAN_HOST_MCP_CONFIG_DIR` env override
/// exists for the test suite so resolution tests don't read the
/// developer's real config.
fn read_config_default() -> Option<PathBuf> {
    let dir = config_dir()?;
    let path = dir.join("lean-host-mcp").join("config.toml");
    let contents = std::fs::read_to_string(&path).ok()?;
    let parsed: ConfigFile = match toml::from_str(&contents) {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "config.toml parse failed; ignoring");
            return None;
        }
    };
    parsed.primary_project
}

fn config_dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LEAN_HOST_MCP_CONFIG_DIR") {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir()
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
        assert!(http_config(&cli.serve).expect("stdio config").is_none());
    }

    #[test]
    fn bind_selects_http_with_default_path() {
        let cli = Cli::parse_from(["lean-host-mcp", "--bind", "127.0.0.1:8765"]);
        let http = http_config(&cli.serve).expect("http config").expect("http selected");
        assert_eq!(http.bind, "127.0.0.1:8765".parse().expect("socket addr"));
        assert_eq!(http.path, DEFAULT_HTTP_PATH);
    }

    #[test]
    fn http_path_without_bind_is_rejected() {
        let cli = Cli::parse_from(["lean-host-mcp", "--http-path", "/mcp"]);
        let err = http_config(&cli.serve).expect_err("http path requires bind");
        assert!(err.to_string().contains("requires --bind"));
    }

    #[test]
    fn non_loopback_bind_is_rejected() {
        let cli = Cli::parse_from(["lean-host-mcp", "--bind", "0.0.0.0:8765"]);
        let err = http_config(&cli.serve).expect_err("non-loopback should fail");
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
