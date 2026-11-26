//! `lean-host-mcp`—Model Context Protocol server hosting Lean 4 via a
//! supervised [`lean-rs-worker`] child.
//!
//! Stdio is the default transport. `--bind` selects Streamable HTTP.
//! `--lake-root` (or `LEAN_HOST_MCP_PROJECT`) names the primary Lake project;
//! the server reads its config files from that root and locks its store.
//! Without it, calls resolve the Lake project above the working directory.

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
use lean_host_mcp::cli::install::{self, InstallArgs};
use lean_host_mcp::cli::install_worker::{self, InstallWorkerArgs};
use lean_host_mcp::cli::processes::{self, DoctorProcessesArgs};
use lean_host_mcp::store::{self, StoreLock};
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

    /// Internal process-bound shared-writer supervisor.
    Writer(lean_host_mcp::shared_writer::WriterArgs),
    /// Resolve the shared project-lock domain without starting runtime work.
    #[command(hide = true)]
    WriterLockPath(lean_host_mcp::shared_writer::WriterLockPathArgs),
    /// Internal transfer of shared read custody to an actual worker.
    Reader(lean_host_mcp::shared_writer::ReaderArgs),
    /// Internal parent-cache read without a parent locked descriptor.
    ReadProxy(lean_host_mcp::shared_writer::ReadProxyArgs),
    #[command(hide = true)]
    WriterExec,
    /// Internal independent owner for a native worker generation.
    #[command(hide = true)]
    NativeGuardian,
    /// Internal guardian-owned execution of one prebuilt native reader.
    #[command(hide = true)]
    NativeReader(lean_host_mcp::native_reader::NativeReaderArgs),
    #[command(hide = true)]
    NativeReaderExec(lean_host_mcp::native_reader::NativeReaderArgs),
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    NativeReaderWait(lean_host_mcp::native_reader::NativeReaderArgs),
    /// Install everything one Lake root needs to serve, and record it in the root's local config.
    Install(InstallArgs),
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
    /// Check the exact registry after draining supported writer launchers.
    WriterRollback {
        #[arg(long)]
        registry: PathBuf,
        #[arg(long)]
        registry_device: u64,
        #[arg(long)]
        registry_inode: u64,
        #[arg(long)]
        confirm_launchers_drained: bool,
    },
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Select and approve the primary Lake project for native calls (also `LEAN_HOST_MCP_PROJECT`).
    /// A per-call project can select this root. Discovery alone cannot approve a native project.
    #[arg(long, env = "LEAN_HOST_MCP_PROJECT")]
    lake_root: Option<PathBuf>,

    /// Directory for durable epoch-bound request results. One broker per directory;
    /// old requests are recovered as evidence, never replayed.
    #[arg(long, env = "LEAN_HOST_MCP_REQUEST_JOURNAL")]
    request_journal: Option<PathBuf>,

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
    if let Err(error) = configure_native_startup(cli.command.as_ref()) {
        eprintln!("lean-host-mcp native settings: {error}");
        return ExitCode::FAILURE;
    }
    match cli.command {
        Some(Command::Install(args)) => match install::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("lean-host-mcp install: {err:#}");
                ExitCode::FAILURE
            }
        },
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
        Some(Command::Doctor {
            command:
                DoctorCommand::WriterRollback {
                    registry,
                    registry_device,
                    registry_inode,
                    confirm_launchers_drained,
                },
        }) => match check_writer_rollback(&registry, registry_device, registry_inode, confirm_launchers_drained) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp doctor writer-rollback: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::NativeReader(args)) => match lean_host_mcp::native_reader::run_cli(&args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("lean-host-mcp native-reader: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::NativeReaderExec(args)) => match lean_host_mcp::native_reader::run_exec(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp native-reader-exec: {error}");
                ExitCode::FAILURE
            }
        },
        #[cfg(target_os = "linux")]
        Some(Command::NativeReaderWait(args)) => match lean_host_mcp::native_reader::run_waiter(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp native-reader-wait: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::NativeGuardian) => match lean_toolchain::native_guardian::run_guardian_cli() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp native-guardian: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::WriterLockPath(args)) => match lean_host_mcp::shared_writer::run_lock_path_cli(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp writer-lock-path: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::ReadProxy(args)) => match lean_host_mcp::shared_writer::run_read_proxy(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp read-proxy: {error}");
                ExitCode::from(lean_host_mcp::shared_writer::reader_failure_exit_code(&error))
            }
        },
        Some(Command::Reader(args)) => match lean_host_mcp::shared_writer::run_reader(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp reader: {error}");
                ExitCode::from(lean_host_mcp::shared_writer::reader_failure_exit_code(&error))
            }
        },
        Some(Command::WriterExec) => match lean_host_mcp::shared_writer::run_exec() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp writer-exec: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Writer(args)) => match lean_host_mcp::shared_writer::run_cli(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lean-host-mcp writer: {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Serve(args)) => run_serve(args),
        None => run_serve(cli.serve),
    }
}

fn configure_native_startup(command: Option<&Command>) -> std::io::Result<()> {
    match command {
        Some(
            Command::Config { .. }
            | Command::Doctor {
                command: DoctorCommand::Processes(_),
            }
            | Command::WriterLockPath(_)
            | Command::WriterExec
            | Command::NativeGuardian,
        ) => return Ok(()),
        Some(Command::InstallWorker(args)) if args.list || args.clean || args.prune => return Ok(()),
        #[cfg(target_os = "linux")]
        Some(Command::NativeReaderWait(_)) => return Ok(()),
        _ => {}
    }
    lean_host_mcp::shared_writer::configure_native_startup()
}

#[cfg(unix)]
fn check_writer_rollback(root: &std::path::Path, device: u64, inode: u64, drained: bool) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if !drained || std::env::var_os("LEAN_HOST_MCP_SOURCE_HANDOFF_CUTOVER").is_some() {
        return Err(std::io::Error::other(
            "disable source-bound admission and confirm supported launchers are drained before rollback",
        ));
    }
    if lean_host_mcp::shared_writer::ownership_registry()? != root {
        return Err(std::io::Error::other(
            "rollback registry differs from the selected native registry",
        ));
    }
    let verify = || {
        let metadata = std::fs::symlink_metadata(root)?;
        if !metadata.is_dir() || (metadata.dev(), metadata.ino()) != (device, inode) {
            return Err(std::io::Error::other("rollback registry identity changed"));
        }
        Ok(())
    };
    verify()?;
    lean_toolchain::native_guardian::collect_closed_owner_records(root, std::time::Duration::from_secs(5))?;
    verify()
}

#[cfg(not(unix))]
fn check_writer_rollback(_root: &std::path::Path, _device: u64, _inode: u64, _drained: bool) -> std::io::Result<()> {
    Err(std::io::Error::other("native writer rollback is unsupported"))
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
    let filter = logging_filter(std::env::var("RUST_LOG").ok().as_deref());
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

fn logging_filter(directives: Option<&str>) -> EnvFilter {
    const DEFAULT: &str = "info,rmcp=warn";
    match directives.map(str::trim) {
        None | Some("info") => EnvFilter::new(DEFAULT),
        Some(value) => EnvFilter::try_new(value).unwrap_or_else(|_| EnvFilter::new(DEFAULT)),
    }
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "the store lock, broker and service must live until the transport returns"
)]
async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let lake_root = args
        .lake_root
        .as_deref()
        .map(|root| {
            root.canonicalize()
                .with_context(|| format!("--lake-root {}", root.display()))
        })
        .transpose()?;
    // The config files of the Lake root, local over tracked. They are the
    // layer beneath env/CLI: each knob is CLI > env > local > tracked > default.
    let file = ConfigFile::load(lake_root.as_deref());
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

    let (max_projects, idle_timeout) = BrokerConfig::pool_from_env_with_file(&file.broker)?;
    let (max_workers, max_workers_per_project) = BrokerConfig::worker_limits_from_file(&file.broker, max_projects)?;
    let max_projects = max_workers; // Legacy internal name; the limit counts worker processes.
    let runtime_config = ProjectRuntimeConfig::from_env_with_file(&file.runtime, max_workers.get())?
        .with_operator_primary(lake_root.as_deref())?;
    let tool_config = resolve_tool_config(&file)?;
    // One server per store. Take the lock after the config checks, so a bad
    // config leaves the store alone, and before the broker, the process record
    // and the journal open.
    let _store = lake_root
        .as_deref()
        .map(|root| StoreLock::acquire(&store::store_dir(root, None), root))
        .transpose()?;
    let transport = if http.is_some() { "http" } else { "stdio" };
    let bind_display = http.as_ref().map(|config| config.bind.to_string());
    let http_path_log = http.as_ref().map(|config| config.path.as_str());

    tracing::info!(
        lake_root = ?lake_root,
        cwd = %cwd.display(),
        max_workers = %max_projects,
        max_workers_per_project = %max_workers_per_project,
        idle_timeout_secs = idle_timeout.as_secs(),
        lean_max_memory_kib = runtime_config.lean_max_memory_kib(),
        import_residue_budget_mib = runtime_config.import_residue_budget_bytes() / (1024 * 1024),
        project_mailbox_capacity = runtime_config.mailbox_capacity(),
        transport = transport,
        bind = bind_display.as_deref(),
        http_path = http_path_log,
        "starting lean-host-mcp",
    );

    let broker = build_broker(
        BrokerConfig {
            env_default: lake_root,
            cwd,
            max_projects,
            max_workers_per_project,
            idle_timeout,
        },
        runtime_config,
    );
    let _process_record = processes::ServerProcessRecord::register(transport, bind_display.clone(), http_path_log)?;
    let mut service = LeanHostService::new(Arc::clone(&broker), tool_config);
    if let Some(path) = args.request_journal {
        service = service.with_request_journal(&path)?;
    }
    let result = if let Some(config) = http {
        transport_http::serve(service, config).await
    } else {
        serve_stdio(service).await
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
async fn serve_stdio(service: LeanHostService) -> anyhow::Result<()> {
    let parent_pid = processes::current_parent_pid();
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
        if processes::parent_lost(
            parent_pid,
            processes::current_parent_pid(),
            processes::process_liveness(parent_pid),
        ) {
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
    let slow_call_warning_millis = tool_env_int::<u64>("LEAN_HOST_MCP_OUTPUT_SLOW_CALL_WARNING_MILLIS")?
        .or(file.output.slow_call_warning_millis)
        .unwrap_or(lean_host_mcp::tools::DEFAULT_SLOW_CALL_WARNING_MILLIS);
    Ok(ToolConfig {
        carrier,
        verbosity,
        output,
        slow_call_warning_millis,
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
#[allow(clippy::expect_used, reason = "tests assert the branch under test directly")]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn logging_defaults_quiet_rmcp_info_but_keep_warnings_and_verbose_opt_in() {
        use std::io::Write;
        use std::sync::Mutex;
        #[derive(Clone, Default)]
        struct Buffer(Arc<Mutex<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .map_err(|_| std::io::Error::other("log buffer poisoned"))?
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for (setting, verbose) in [(None, false), (Some("info"), false), (Some("info,rmcp=debug"), true)] {
            let buffer = Buffer::default();
            let writer = buffer.clone();
            let subscriber = fmt()
                .without_time()
                .with_ansi(false)
                .with_env_filter(logging_filter(setting))
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(target: "rmcp::service", "session noise");
                tracing::debug!(target: "rmcp::service", "verbose detail");
                tracing::warn!(target: "rmcp::service", "transport warning");
                tracing::error!(target: "rmcp::service", "transport error");
                tracing::info!(target: "lean_host_mcp::request_log", "request summary");
            });
            let output = String::from_utf8(buffer.0.lock().expect("buffer").clone()).expect("utf8");
            assert_eq!(output.contains("session noise"), verbose);
            assert_eq!(output.contains("verbose detail"), verbose);
            assert!(output.contains("transport warning"));
            assert!(output.contains("transport error"));
            assert!(output.contains("request summary"));
        }
    }

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
