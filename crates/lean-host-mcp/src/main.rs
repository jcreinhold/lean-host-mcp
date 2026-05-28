//! `lean-host-mcp`—Model Context Protocol server hosting Lean 4 via a
//! supervised [`lean-rs-worker`] child.
//!
//! Stdio transport. Wire into Claude Code / any MCP client by pointing the
//! `command` at the built binary. With a Lake project visible from the
//! invocation directory (or one set via `LEAN_HOST_MCP_PROJECT` /
//! `~/.config/lean-host-mcp/config.toml`), no flags are required.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

use lean_host_mcp::cli::install_worker::{self, InstallWorkerArgs};
use lean_host_mcp::{BrokerConfig, LeanHostService, ProjectBroker, default_cache_dir};

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

    /// Directory for the `SQLite` declaration index. Defaults to
    /// `$XDG_CACHE_HOME/lean-host-mcp` (or `$HOME/.cache/lean-host-mcp`).
    #[arg(long, env = "LEAN_HOST_MCP_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
}

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

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let cache_dir = args.cache_dir.unwrap_or_else(default_cache_dir);
    let env_default = args.lake_root;
    let config_default = read_config_default();
    let cwd = std::env::current_dir()?;
    let (max_projects, idle_timeout, semantic_permits) = BrokerConfig::pool_from_env()?;

    tracing::info!(
        env_default = ?env_default,
        config_default = ?config_default,
        cwd = %cwd.display(),
        cache_dir = %cache_dir.display(),
        max_projects = %max_projects,
        idle_timeout_secs = idle_timeout.as_secs(),
        semantic_permits = %semantic_permits,
        "starting lean-host-mcp",
    );

    let broker = ProjectBroker::new(BrokerConfig {
        cache_dir,
        config_default,
        env_default,
        cwd,
        max_projects,
        idle_timeout,
        semantic_permits,
    });
    let service = LeanHostService::new(broker);
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
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
