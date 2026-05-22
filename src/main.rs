//! `lean-host-mcp` — Model Context Protocol server hosting Lean 4 in-process
//! via [`lean-rs`].
//!
//! Stdio transport. Wire into Claude Code / any MCP client by pointing the
//! `command` at the built binary and passing `--lake-root <path>`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

use lean_host_mcp::{LeanHostService, SessionHost};

/// Stdio MCP server that hosts a Lean 4 environment in-process.
#[derive(Debug, Parser)]
#[command(name = "lean-host-mcp", version, about)]
struct Cli {
    /// Lake project root. The project must depend on the `lean-rs-host` shim
    /// library so the 26 + 4 `lean_rs_host_*` symbols are present in its
    /// build output. v0.1 does not ship a self-contained shim; see the
    /// repo README for the prerequisite.
    #[arg(long, env = "LEAN_HOST_MCP_LAKE_ROOT")]
    lake_root: PathBuf,

    /// Lean package name to load capabilities from (the `package` keyword in
    /// the Lake `lakefile.lean`). Defaults to the directory name of
    /// `--lake-root`.
    #[arg(long, env = "LEAN_HOST_MCP_PACKAGE")]
    package: Option<String>,

    /// Lean library name within the package whose dylib carries the
    /// `lean_rs_host_*` symbols (the `lean_lib` declaration). Defaults to a
    /// PascalCase of the package name.
    #[arg(long, env = "LEAN_HOST_MCP_LIBRARY")]
    library: Option<String>,

    /// Module imports for every fresh session. Comma-separated. The package's
    /// own modules are reachable here once Lake has built them.
    #[arg(long, env = "LEAN_HOST_MCP_IMPORTS", value_delimiter = ',')]
    imports: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "lean-host-mcp exited with error");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let package = cli.package.clone().unwrap_or_else(|| default_package(&cli.lake_root));
    let library = cli.library.clone().unwrap_or_else(|| pascal_case(&package));

    tracing::info!(
        lake_root = %cli.lake_root.display(),
        package = %package,
        library = %library,
        imports = ?cli.imports,
        "starting lean-host-mcp",
    );

    let host = SessionHost::spawn(cli.lake_root, package, library, cli.imports)?;
    let service = LeanHostService::new(host);
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn default_package(lake_root: &std::path::Path) -> String {
    lake_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lean_project")
        .replace('-', "_")
}

fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}
