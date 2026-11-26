//! `lean-host-mcp install`: make one Lake root ready to serve.
//!
//! For `--lake-root <root>` it reads the toolchain from `<root>/lean-toolchain`
//! and, under the install directory (`--dir`, default
//! `<root>/.local/lean-host-mcp`):
//!
//! - installs the SDK writer at `bin/lean-rs-writer`;
//! - copies this executable to `bin/lean-host-mcp`;
//! - installs the worker for the toolchain in `workers/`, by the
//!   `install-worker` code path;
//! - creates the private server store `store/` (mode 0700).
//!
//! Without `--dir`, `LEAN_HOST_MCP_WORKERS_DIR` and `LEAN_HOST_MCP_STORE_DIR`
//! and an earlier install's paths still apply, as they do for the server. The
//! command then records the absolute paths in the `[install]` table of
//! `<root>/.local/lean-host-mcp/config.toml`, the ignored local config file. It
//! never writes a tracked file.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
use clap::Args;

use super::install_worker::{self, Freshness, WriterInputs};
use crate::config_file;
use crate::store;
use crate::toolchain::{self, ToolchainId, shell_word};

/// File name of the SDK writer helper.
const WRITER_FILE_NAME: &str = "lean-rs-writer";

/// File name of the installed server executable.
const SERVER_FILE_NAME: &str = "lean-host-mcp";

/// Comment at the top of the local config file. Install rewrites the file, so
/// it keeps other keys but not comments.
const LOCAL_FILE_HEADER: &str = "\
# Local lean-host-mcp settings for this Lake root; Git ignores this file.
# Keys here win over lean-host-mcp.toml at the root. `lean-host-mcp install`
# rewrites [install] and keeps other keys, but not comments.

";

/// Flags for `install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Lake root to install for; its `lean-toolchain` picks the worker.
    #[arg(long, value_name = "DIR")]
    pub lake_root: PathBuf,

    /// Install directory for `bin/`, `workers/` and `store/`; defaults to
    /// `<root>/.local/lean-host-mcp`.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Rebuild the worker and the writer even when they are current.
    #[arg(long)]
    pub force: bool,

    /// Standalone SDK writer to install instead of building `lean-rs-writer`.
    #[arg(long)]
    pub standalone_writer: Option<PathBuf>,

    /// Extracted lean-rs-host package containing the pinned published shim sources.
    #[arg(long)]
    pub shim_source: Option<PathBuf>,

    /// Workspace root to build the worker from. Defaults to the workspace
    /// this binary was compiled in, else the published crate.
    #[arg(long, value_name = "DIR")]
    pub source_dir: Option<PathBuf>,
}

/// Where one install puts things. All paths are absolute.
#[derive(Debug)]
struct Plan {
    root: PathBuf,
    toolchain: ToolchainId,
    bin: PathBuf,
    workers_dir: PathBuf,
    store_dir: PathBuf,
}

impl Plan {
    /// Resolve the plan for `args`, with the worker and store env vars'
    /// values passed in.
    fn resolve(args: &InstallArgs, workers_env: Option<OsString>, store_env: Option<OsString>) -> anyhow::Result<Self> {
        let root = args
            .lake_root
            .canonicalize()
            .with_context(|| format!("--lake-root {}", args.lake_root.display()))?;
        let toolchain = ToolchainId::from_lake_root(&root)?;
        let dir = match args.dir.as_deref() {
            Some(dir) => std::path::absolute(dir)?,
            None => config_file::local_dir(&root),
        };
        // `--dir` is a CLI flag and beats the env vars and the config files.
        let flag = |name: &str| args.dir.as_ref().map(|_| dir.join(name));
        let workers_dir = toolchain::workers_dir_with(&root, flag("workers").as_deref(), workers_env);
        let store_dir = store::store_dir_with(&root, flag("store").as_deref(), store_env);
        Ok(Self {
            bin: dir.join("bin"),
            workers_dir: std::path::absolute(workers_dir)?,
            store_dir: std::path::absolute(store_dir)?,
            root,
            toolchain,
        })
    }

    fn binary(&self) -> PathBuf {
        self.bin.join(SERVER_FILE_NAME)
    }

    fn writer(&self) -> PathBuf {
        self.bin.join(WRITER_FILE_NAME)
    }
}

/// Entry point invoked from `main.rs`.
///
/// # Errors
///
/// Returns an error when the Lake root has no readable `lean-toolchain`, when
/// the writer or worker install fails, or when a file cannot be written.
pub fn run(args: &InstallArgs) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("cannot find this executable")?;
    let plan = Plan::resolve(
        args,
        std::env::var_os(toolchain::WORKERS_DIR_ENV),
        std::env::var_os(store::STORE_DIR_ENV),
    )?;
    let config = install(&plan, args, &exe, |workers, id, writer| {
        let source = install_worker::resolve_worker_source(args.source_dir.as_deref())?;
        let inputs = WriterInputs {
            shim_source: args.shim_source.clone(),
            writer: Some(writer.to_path_buf()),
        };
        install_worker::install_one(workers, id, &source, &inputs).map(drop)
    })?;
    let binary = plan.binary();
    println!(
        "installed lean-host-mcp for {} (leanprover/lean4:{})",
        plan.root.display(),
        plan.toolchain
    );
    println!("  server   {}", binary.display());
    println!("  workers  {}", plan.workers_dir.display());
    println!("  store    {}", plan.store_dir.display());
    println!("  config   {}", config.display());
    println!(
        "serve with: {} --lake-root {}",
        shell_word(&binary.to_string_lossy()),
        shell_word(&plan.root.to_string_lossy())
    );
    Ok(())
}

/// Carry out `plan`. `build_worker` installs the worker for a toolchain in a
/// worker directory, qualifying it with the given writer; tests replace it.
/// Returns the local config file it wrote.
fn install(
    plan: &Plan,
    args: &InstallArgs,
    exe: &Path,
    build_worker: impl FnOnce(&Path, &ToolchainId, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    // Read the local file first, so a broken one stops install before it
    // builds anything.
    let local = read_local_config(&plan.root)?;
    config_file::ensure_local_dir(&plan.root)?;
    fs::create_dir_all(&plan.bin).with_context(|| format!("creating {}", plan.bin.display()))?;

    let writer = plan.writer();
    install_writer(&writer, exe, args.standalone_writer.as_deref(), args.force)?;
    copy_executable(exe, &plan.binary())?;

    fs::create_dir_all(&plan.workers_dir).with_context(|| format!("creating {}", plan.workers_dir.display()))?;
    match install_worker::worker_freshness_in(&plan.workers_dir, &plan.toolchain) {
        Freshness::Current if !args.force => {
            println!("worker for {}: current (use --force to rebuild)", plan.toolchain);
        }
        Freshness::Current | Freshness::Absent | Freshness::Stale(_) => {
            build_worker(&plan.workers_dir, &plan.toolchain, &writer)?;
        }
    }

    store::create_store_dir(&plan.store_dir, &plan.root)
        .with_context(|| format!("creating store {}", plan.store_dir.display()))?;
    write_local_config(
        &plan.root,
        local,
        &[
            ("binary", &plan.binary()),
            ("workers_dir", &plan.workers_dir),
            ("store_dir", &plan.store_dir),
        ],
    )
}

/// Put the SDK writer at `dest`: `--standalone-writer` when given, else a
/// `lean-rs-writer` of the pinned SDK version beside this executable, else one
/// built with `cargo install`. A current writer already at `dest` stays unless
/// `force` is set.
fn install_writer(dest: &Path, exe: &Path, standalone: Option<&Path>, force: bool) -> anyhow::Result<()> {
    if let Some(source) = standalone {
        return copy_executable(source, dest);
    }
    if !force && writer_is_current(dest) {
        return Ok(());
    }
    let sibling = exe.with_file_name(WRITER_FILE_NAME);
    if writer_is_current(&sibling) {
        return copy_executable(&sibling, dest);
    }
    build_writer(dest)
}

/// Whether `path` runs and reports the SDK writer version this host pins.
fn writer_is_current(path: &Path) -> bool {
    Command::new(path).arg("--version").output().is_ok_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).split_whitespace().last() == Some(lean_toolchain::VERSION)
    })
}

/// `cargo install` the pinned `lean-rs-writer` into a scratch root beside
/// `dest`, then move only the binary to `dest`.
fn build_writer(dest: &Path) -> anyhow::Result<()> {
    let bin = dest.parent().context("writer destination has no directory")?;
    let scratch = tempfile::Builder::new().prefix(".cargo-install-").tempdir_in(bin)?;
    let version = lean_toolchain::VERSION;
    println!("==> installing {WRITER_FILE_NAME} {version} (crates.io)");
    let status = Command::new("cargo")
        .args(["install", "--locked", "--no-track", "--version"])
        .arg(format!("={version}"))
        .arg("--root")
        .arg(scratch.path())
        .arg(WRITER_FILE_NAME)
        .status()
        .context("cannot run cargo")?;
    anyhow::ensure!(
        status.success(),
        "cargo install {WRITER_FILE_NAME}@={version} failed with status {status}; \
         a Rust toolchain and network access are required, or pass --standalone-writer"
    );
    fs::rename(scratch.path().join("bin").join(WRITER_FILE_NAME), dest)?;
    Ok(())
}

/// Copy the executable `source` to `dest` with mode 0755, replacing `dest`
/// by rename so a running copy keeps its file. Copying a file onto itself
/// does nothing.
fn copy_executable(source: &Path, dest: &Path) -> anyhow::Result<()> {
    if let (Ok(from), Ok(to)) = (fs::metadata(source), fs::metadata(dest))
        && (from.dev(), from.ino()) == (to.dev(), to.ino())
    {
        return Ok(());
    }
    let dir = dest.parent().context("destination has no directory")?;
    let mut temp = tempfile::Builder::new().prefix(".install-").tempfile_in(dir)?;
    let mut from = fs::File::open(source).with_context(|| format!("opening {}", source.display()))?;
    io::copy(&mut from, temp.as_file_mut())?;
    temp.as_file().set_permissions(fs::Permissions::from_mode(0o755))?;
    temp.as_file().sync_all()?;
    temp.persist(dest)?;
    Ok(())
}

/// The local config file of `root` as a table: empty when the file is
/// missing, an error when it does not parse, so install never replaces it.
fn read_local_config(root: &Path) -> anyhow::Result<toml::Table> {
    let path = config_file::local_file(root);
    match fs::read_to_string(&path) {
        Ok(text) => text.parse::<toml::Table>().with_context(|| {
            format!(
                "{} is not valid TOML; fix or remove it, then rerun install",
                path.display()
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `table`, the local config file of `root`, with its `[install]` table
/// set to `paths`.
fn write_local_config(root: &Path, mut table: toml::Table, paths: &[(&str, &Path)]) -> anyhow::Result<PathBuf> {
    let path = config_file::local_file(root);
    let mut install = toml::Table::new();
    for (key, value) in paths {
        let value = value
            .to_str()
            .with_context(|| format!("{} is not UTF-8; TOML cannot hold it", value.display()))?;
        install.insert((*key).to_owned(), toml::Value::String(value.to_owned()));
    }
    table.insert("install".to_owned(), toml::Value::Table(install));
    let dir = path.parent().context("config file has no directory")?;
    let mut temp = tempfile::Builder::new().prefix(".config-").tempfile_in(dir)?;
    io::Write::write_all(temp.as_file_mut(), format!("{LOCAL_FILE_HEADER}{table}").as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_file::ConfigFile;
    use crate::toolchain::WORKER_FILE_NAME;

    struct Fixture {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        root: PathBuf,
        exe: PathBuf,
        writer: PathBuf,
    }

    fn fixture() -> anyhow::Result<Fixture> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().canonicalize()?;
        let root = base.join("project");
        fs::create_dir(&root)?;
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.36.0\n")?;
        fs::write(root.join("lean-host-mcp.toml"), "[broker]\nmax_workers = 2\n")?;
        let exe = base.join("built-lean-host-mcp");
        fs::write(&exe, b"server")?;
        let writer = base.join("built-writer");
        fs::write(&writer, b"writer")?;
        Ok(Fixture {
            _tmp: tmp,
            base,
            root,
            exe,
            writer,
        })
    }

    fn args(fixture: &Fixture, dir: Option<PathBuf>) -> InstallArgs {
        InstallArgs {
            lake_root: fixture.root.clone(),
            dir,
            force: false,
            standalone_writer: Some(fixture.writer.clone()),
            shim_source: None,
            source_dir: None,
        }
    }

    /// Install with a fake worker build that records its inputs.
    fn run_install(args: &InstallArgs, exe: &Path) -> anyhow::Result<(Plan, Vec<(PathBuf, String, PathBuf)>)> {
        let plan = Plan::resolve(args, None, None)?;
        let mut builds = Vec::new();
        install(&plan, args, exe, |workers, id, writer| {
            builds.push((workers.to_path_buf(), id.to_string(), writer.to_path_buf()));
            let slot = workers.join(id.as_str());
            fs::create_dir_all(&slot)?;
            fs::write(slot.join(WORKER_FILE_NAME), b"worker")?;
            Ok(())
        })?;
        Ok((plan, builds))
    }

    fn mode(path: &Path) -> anyhow::Result<u32> {
        Ok(fs::metadata(path)?.permissions().mode() & 0o777)
    }

    #[test]
    fn install_records_its_paths_in_the_local_file_only() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let root = &fixture.root;
        let tracked = fs::read(root.join("lean-host-mcp.toml"))?;
        config_file::ensure_local_dir(root)?;
        fs::write(config_file::local_file(root), "[runtime]\nworker_restart_limit = 5\n")?;

        let (_, builds) = run_install(&args(&fixture, None), &fixture.exe)?;

        let local = config_file::local_dir(root);
        assert_eq!(
            builds,
            [(
                local.join("workers"),
                "v4.36.0".to_owned(),
                local.join("bin/lean-rs-writer")
            )]
        );
        let text = fs::read_to_string(config_file::local_file(root))?;
        assert!(text.starts_with(LOCAL_FILE_HEADER), "{text}");
        let config = ConfigFile::load(Some(root));
        let install = &config.install;
        assert_eq!(
            install.binary.as_deref(),
            Some(local.join("bin/lean-host-mcp").as_path())
        );
        assert_eq!(install.workers_dir(root), Some(local.join("workers")));
        assert_eq!(install.store_dir(root), Some(local.join("store")));
        assert_eq!(
            config.runtime.worker_restart_limit,
            Some(5),
            "install keeps other local keys"
        );
        assert_eq!(config.broker.max_workers, Some(2), "the tracked file still applies");

        assert_eq!(fs::read(local.join("bin/lean-host-mcp"))?, b"server");
        assert_eq!(mode(&local.join("bin/lean-host-mcp"))?, 0o755);
        assert_eq!(fs::read(local.join("bin/lean-rs-writer"))?, b"writer");
        assert_eq!(mode(&local.join("store"))?, 0o700);
        assert_eq!(fs::read_to_string(local.join(".gitignore"))?, "*\n");
        assert_eq!(
            fs::read(root.join("lean-host-mcp.toml"))?,
            tracked,
            "tracked file unchanged"
        );
        let mut top: Vec<_> = fs::read_dir(root)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<_>>()?;
        top.sort();
        assert_eq!(top, [".local", "lean-host-mcp.toml", "lean-toolchain"]);
        Ok(())
    }

    #[test]
    fn install_dir_holds_everything_but_the_local_config() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let dir = fixture.base.join("tools");
        let (plan, builds) = run_install(&args(&fixture, Some(dir.clone())), &fixture.exe)?;
        assert_eq!(plan.binary(), dir.join("bin/lean-host-mcp"));
        assert_eq!(builds.len(), 1);
        assert_eq!(builds.first().map(|build| build.0.clone()), Some(dir.join("workers")));
        let install = ConfigFile::load(Some(&fixture.root)).install;
        assert_eq!(install.workers_dir(&fixture.root), Some(dir.join("workers")));
        assert_eq!(install.store_dir(&fixture.root), Some(dir.join("store")));
        assert!(config_file::local_file(&fixture.root).is_file());

        // Without `--dir`, a reinstall uses the recorded worker and store paths,
        // as the server does.
        let again = Plan::resolve(&args(&fixture, None), None, None)?;
        assert_eq!(again.workers_dir, dir.join("workers"));
        assert_eq!(again.store_dir, dir.join("store"));
        assert_eq!(again.bin, config_file::local_dir(&fixture.root).join("bin"));
        Ok(())
    }

    #[test]
    fn install_refuses_to_rewrite_a_broken_local_file() -> anyhow::Result<()> {
        let fixture = fixture()?;
        config_file::ensure_local_dir(&fixture.root)?;
        fs::write(config_file::local_file(&fixture.root), "[runtime\n")?;
        let Err(error) = run_install(&args(&fixture, None), &fixture.exe) else {
            anyhow::bail!("install rewrote a broken local file");
        };
        assert!(error.to_string().contains("is not valid TOML"), "{error}");
        assert_eq!(
            fs::read_to_string(config_file::local_file(&fixture.root))?,
            "[runtime\n"
        );
        let local = config_file::local_dir(&fixture.root);
        assert!(!local.join("bin").exists(), "install stopped before it built anything");
        Ok(())
    }
}
