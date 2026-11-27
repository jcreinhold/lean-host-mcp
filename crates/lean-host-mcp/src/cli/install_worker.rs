//! `lean-host-mcp install-worker`: build the worker binary for one Lean
//! toolchain and place it in a worker directory.
//!
//! The directory is `--workers-dir`, else the one
//! [`crate::toolchain::workers_dir`] picks for `--lake-root`. Without either,
//! `LEAN_HOST_MCP_WORKERS_DIR` must name it.
//!
//! Actions:
//!
//! - `--toolchain <id>`: build for one toolchain (always overwrites).
//! - no flag / `--auto`: scan `<elan home>/toolchains/leanprover--lean4---*`
//!   and build for any that are missing or stale (protocol mismatch, header
//!   drift, failed/absent smoke, built by another release); `--force`
//!   rebuilds current ones too. The elan home is [`toolchain::elan_home`],
//!   which with `--lake-root` includes the root's recorded
//!   `[install] elan_home`.
//!
//! A build uses this executable as the SDK writer for the worker's shims, and
//! runs the worker's smoke test, which is native work. With `--lake-root` its
//! state goes in that root's store; without, set
//! `LEAN_RS_WORKER_OWNERS_DIRECTORY`.
//! - `--list`: print a table of currently-installed workers, including a
//!   `protocol` column that reports wire compatibility.
//! - `--clean [--toolchain <id>]`: remove all installed workers, or just one.
//! - `--prune`: remove only unservable workers (outside the supported window,
//!   or with a failed smoke test), keeping servable-but-stale ones.
//!
//! The worker is always compiled locally per toolchain (its `build.rs` bakes an
//! absolute rpath, so binaries don't travel), with `LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>`
//! set so the right rpath is baked in. Where the worker *source* comes from is
//! decided once, internally, by `resolve_worker_source`: a local checkout
//! (`cargo build -p lean-host-mcp-worker`) when this binary was built from one,
//! otherwise the published crate (`cargo install lean-host-mcp-worker`). Callers
//! of `install-worker` never choose; `--source-dir` is the override for the rare
//! case of a checkout that moved after the binary was built. `cargo` output
//! streams to stdout/stderr unchanged so the user sees real build errors.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use clap::Args;

use crate::config_file::{self, ConfigFile};
use crate::store;
use crate::toolchain::{
    self, ToolchainId, WORKER_FILE_NAME, WORKERS_DIR_ENV, WindowVerdict, WorkerBinary, WorkerSidecar, hash_lean_header,
};

/// Flags for `install-worker`.
///
/// The *action* (`--auto`, `--list`, `--clean`, `--prune`) is mutually
/// exclusive; `--toolchain` is a target modifier that scopes the install or
/// `--clean` to one toolchain (validated in [`run`]).
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("action")
        .args(["auto", "list", "clean", "prune"])
))]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is a distinct CLI flag; the action flags are made \
              mutually exclusive by the clap ArgGroup, and modeling them as one \
              enum would fight clap's flag derivation and the established \
              --auto/--list UX"
)]
pub struct InstallWorkerArgs {
    /// Operate on a single toolchain (e.g. `v4.30.0` or
    /// `leanprover/lean4:v4.30.0`): build+install it, or, with `--clean`,
    /// remove just it. Omit it to act on every discovered/installed toolchain.
    #[arg(long, value_name = "ID")]
    pub toolchain: Option<String>,

    /// Scan the elan home's `toolchains` and install a worker for every supported
    /// Lean toolchain that is missing or stale (host-version skew, header
    /// drift, or a failed/absent smoke record). This is the default when no
    /// action flag is supplied. Use `--force` to rebuild current ones too.
    #[arg(long)]
    pub auto: bool,

    /// Print a table of currently-installed worker binaries.
    #[arg(long)]
    pub list: bool,

    /// Remove installed workers: all of them, or just `--toolchain <id>`.
    /// Workers are rebuildable artifacts, so this only deletes from the
    /// install root; it never touches source.
    #[arg(long)]
    pub clean: bool,

    /// Remove only *unservable* workers — those outside the supported window
    /// or with a failed runtime smoke test. Servable-but-stale workers (header
    /// drift, host skew) are kept; rebuild them with `--auto`.
    #[arg(long)]
    pub prune: bool,

    /// Rebuild even workers that are already current. Applies to `--auto` and
    /// `--toolchain` installs; ignored by `--list`/`--clean`/`--prune`.
    #[arg(long)]
    pub force: bool,

    /// Worker directory containing per-toolchain generations. It beats
    /// `LEAN_HOST_MCP_WORKERS_DIR` and the `--lake-root` config.
    #[arg(long, value_name = "DIR")]
    pub workers_dir: Option<PathBuf>,

    /// Lake root whose worker directory to use: `LEAN_HOST_MCP_WORKERS_DIR`,
    /// else its `[install] workers_dir`, else `<root>/.local/lean-host-mcp/workers`.
    #[arg(long, value_name = "DIR")]
    pub lake_root: Option<PathBuf>,

    /// Workspace root to build from. Defaults to the workspace this
    /// binary was compiled in; useful when the installed binary lives
    /// somewhere other than the source tree.
    #[arg(long, value_name = "DIR")]
    pub source_dir: Option<PathBuf>,
}

/// Entry point invoked from `main.rs`.
///
/// # Errors
///
/// Bubbles up filesystem / `cargo` failures as `anyhow::Error`. Returns
/// a non-zero exit-code-equivalent error when any auto install fails.
pub fn run(args: &InstallWorkerArgs) -> anyhow::Result<()> {
    check_arg_combination(args)?;
    let lake_root = args
        .lake_root
        .as_deref()
        .map(|root| {
            root.canonicalize()
                .map_err(|e| anyhow::anyhow!("--lake-root {}: {e}", root.display()))
        })
        .transpose()?;
    // Find the toolchains where install found them, whatever `HOME` is now.
    if let Some(root) = lake_root.as_deref()
        && let Some(home) = ConfigFile::load(Some(root)).install.elan_home(root)
    {
        toolchain::use_recorded_elan_home(&home)?;
    }
    let root = workers_root(args, lake_root.as_deref())?;
    if args.list {
        return run_list(&root);
    }
    if args.prune {
        return prune_in(&root);
    }
    if args.clean {
        // `--clean --toolchain x` removes one; `--clean` alone removes all.
        let target = args.toolchain.as_deref().map(ToolchainId::parse).transpose()?;
        return clean_in(&root, target.as_ref());
    }
    // Default action (no flag) and `--auto` both scan and install. Check where
    // the smoke test keeps its native state before a long build.
    use_native_state(lake_root.as_deref())?;
    let source = resolve_worker_source(args.source_dir.as_deref())?;
    // This executable serves the SDK writer that builds the worker's shims.
    let writer = std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot find this executable: {e}"))?;
    if let Some(raw) = args.toolchain.as_deref() {
        // `check_arg_combination` ruled out `--toolchain --auto`, so this is a
        // single-toolchain install (always overwrites — no freshness check).
        let id = ToolchainId::parse(raw)?;
        install_one(&root, &id, &source, &writer)?;
        return Ok(());
    }
    run_auto(&root, &source, &writer, args.force)
}

/// Keep the native state of a worker's smoke test in the store of the
/// canonical Lake root `lake_root`, creating the store. Without a root,
/// `LEAN_RS_WORKER_OWNERS_DIRECTORY` must name the owner directory.
fn use_native_state(lake_root: Option<&Path>) -> anyhow::Result<()> {
    match lake_root {
        Some(root) => {
            let dir = store::store_dir(root, None);
            store::create_store_dir(&dir, root)
                .and_then(|()| store::use_for_native_state(&dir))
                .map_err(|e| anyhow::anyhow!("cannot keep native state in the store {}: {e}", dir.display()))?;
        }
        None => {
            store::owner_directory()?;
        }
    }
    Ok(())
}

/// The worker directory this run acts on. With the canonical `lake_root` it
/// is the one the server uses for that root; without, `--workers-dir` or
/// `LEAN_HOST_MCP_WORKERS_DIR` must name it.
fn workers_root(args: &InstallWorkerArgs, lake_root: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(root) = lake_root {
        return Ok(toolchain::workers_dir(root, args.workers_dir.as_deref()));
    }
    args.workers_dir
        .clone()
        .or_else(|| config_file::env_dir(WORKERS_DIR_ENV))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no worker directory; pass --lake-root <project> or --workers-dir <dir>, or set {WORKERS_DIR_ENV}"
            )
        })
}

/// Validate the `--toolchain` target modifier against the chosen action. The
/// action flags are made mutually exclusive by clap's `ArgGroup`; `--toolchain`
/// sits outside it (it scopes an install or `--clean`), so its illegal pairings
/// are checked here. Pure — no filesystem or build side effects, so the rules
/// are unit-testable without the install root.
fn check_arg_combination(args: &InstallWorkerArgs) -> anyhow::Result<()> {
    if args.toolchain.is_some() {
        if args.auto {
            return Err(anyhow::anyhow!(
                "--toolchain selects one toolchain and --auto selects all; pass one or the other"
            ));
        }
        if args.list {
            return Err(anyhow::anyhow!(
                "--toolchain is not valid with --list; --list always shows every installed worker"
            ));
        }
        if args.prune {
            return Err(anyhow::anyhow!(
                "--toolchain is not valid with --prune; use `--clean --toolchain <id>` to remove one"
            ));
        }
    }
    Ok(())
}

fn run_list(root: &Path) -> anyhow::Result<()> {
    if !root.is_dir() {
        println!("(no workers installed under {})", root.display());
        return Ok(());
    }
    let mut rows: Vec<ListRow> = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let id_path = entry.path();
        if !id_path.is_dir() {
            continue;
        }
        let Some(id) = id_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let selected = WorkerBinary::selected_dir(&id_path)?;
        let bin = selected.join(WORKER_FILE_NAME);
        if !bin.is_file() {
            continue;
        }
        let meta = fs::metadata(&bin)?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let sha = sha256_prefix(&bin, 8)?;
        // Window + provenance status, derived from the same sources the open
        // gate uses: the pin's window verdict and the sidecar digest re-hashed
        // against the toolchain's current lean.h.
        let parsed = ToolchainId::parse(id).ok();
        let support = parsed.as_ref().map_or("unknown", |t| t.window_verdict().label());
        // Semantic sort key (rc before its release), reusing the same ordering
        // the open gate uses; fall back to the unknown bucket keyed on the raw
        // directory name when it does not parse as a toolchain id.
        let sort_key = parsed
            .as_ref()
            .map_or_else(|| (1, (0, 0, 0, 0, 0), id.to_owned()), ToolchainId::sort_key);
        let current = parsed
            .as_ref()
            .and_then(|t| t.elan_dir().ok())
            .and_then(|dir| hash_lean_header(&dir).ok());
        // One sidecar load feeds build, runtime, and protocol compatibility.
        let sidecar = WorkerSidecar::load(&selected);
        let header = sidecar
            .as_ref()
            .map_or("unknown", |s| s.header_status(current.as_deref()));
        let smoke = sidecar.as_ref().map_or("untested", WorkerSidecar::smoke_status);
        let protocol = sidecar.as_ref().map_or("unknown", WorkerSidecar::protocol_status);
        rows.push(ListRow {
            id: id.to_owned(),
            path: bin,
            support,
            header,
            smoke,
            protocol,
            size: meta.len(),
            mtime,
            sha,
            sort_key,
        });
    }
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    println!(
        "{:<28}  {:<14}  {:<9}  {:<9}  {:<9}  {:>10}  {:<24}  sha256",
        "toolchain", "support", "build", "runtime", "protocol", "size", "built"
    );
    for row in &rows {
        let mtime = humantime::format_rfc3339_seconds_or_fallback(row.mtime);
        let size = format_mib(row.size);
        println!(
            "{:<28}  {:<14}  {:<9}  {:<9}  {:<9}  {:>10}  {:<24}  {}",
            row.id, row.support, row.header, row.smoke, row.protocol, size, mtime, row.sha
        );
    }
    Ok(())
}

struct ListRow {
    id: String,
    #[allow(
        dead_code,
        reason = "path is informational; not yet printed but useful for future flags"
    )]
    path: PathBuf,
    support: &'static str,
    header: &'static str,
    smoke: &'static str,
    protocol: &'static str,
    size: u64,
    mtime: SystemTime,
    sha: String,
    /// Semantic ordering key (rc before its release); see [`ToolchainId::sort_key`].
    sort_key: (u8, (u32, u32, u32, u8, u32), String),
}

fn run_auto(root: &Path, source: &WorkerSource, writer: &Path, force: bool) -> anyhow::Result<()> {
    let toolchains = discover_elan_toolchains()?;
    if toolchains.is_empty() {
        let home = toolchain::elan_home().map_or_else(|| "$ELAN_HOME".to_owned(), |home| home.display().to_string());
        println!("(no Lean toolchains found under {home}/toolchains)");
        return Ok(());
    }
    let mut failed = false;
    for id in toolchains {
        match auto_decision(root, &id, force) {
            AutoDecision::SkipUnsupported => println!("{id}: skipped (outside supported window)"),
            AutoDecision::SkipCurrent => {
                println!("{id}: current; skipping (use --force to rebuild)");
            }
            AutoDecision::Install(reason) => match install_one(root, &id, source, writer) {
                Ok(_) => println!("{id}: installed ({reason})"),
                Err(err) => {
                    eprintln!("{id}: failed: {err}");
                    failed = true;
                }
            },
        }
    }
    if failed {
        Err(anyhow::anyhow!("one or more --auto installs failed"))
    } else {
        Ok(())
    }
}

/// What `--auto` should do for one discovered toolchain.
enum AutoDecision {
    /// (Re)build it; the `&str` is the reason, shown to the user.
    Install(&'static str),
    /// A current worker is already installed — skip unless `--force`.
    SkipCurrent,
    /// Outside the supported window: building it would only fail the
    /// header-digest check, so don't attempt it.
    SkipUnsupported,
}

fn auto_decision(root: &Path, id: &ToolchainId, force: bool) -> AutoDecision {
    if matches!(id.window_verdict(), WindowVerdict::OutOfWindow { .. }) {
        return AutoDecision::SkipUnsupported;
    }
    match worker_freshness_in(root, id) {
        Freshness::Absent => AutoDecision::Install("new"),
        Freshness::Stale(reason) => AutoDecision::Install(reason),
        Freshness::Current => {
            if force {
                AutoDecision::Install("forced")
            } else {
                AutoDecision::SkipCurrent
            }
        }
    }
}

/// Whether the worker already installed under the selected root for
/// `id` is current, stale, or absent. Mirrors the runtime open-gate's provenance
/// checks ([`WorkerBinary::resolve_ready_with`]), and also counts a worker that
/// another `lean-host-mcp` release built as stale. The worker and host crates
/// release together, and a release can keep the protocol version, so the
/// gate still serves such a worker but install replaces it.
pub(super) enum Freshness {
    Absent,
    Current,
    Stale(&'static str),
}

/// Freshness of the worker for `id` in the worker directory `root`.
pub(super) fn worker_freshness_in(root: &Path, id: &ToolchainId) -> Freshness {
    let Ok(dir) = WorkerBinary::selected_dir(&root.join(id.as_str())) else {
        return Freshness::Stale("invalid generation selector");
    };
    if !dir.join(WORKER_FILE_NAME).is_file() {
        return Freshness::Absent;
    }
    let Some(sidecar) = WorkerSidecar::load(&dir) else {
        return Freshness::Stale("no provenance record");
    };
    // Header drift trumps everything: the toolchain's lean.h changed under it.
    if let Ok(elan) = id.elan_dir()
        && let Ok(current) = hash_lean_header(&elan)
        && !sidecar.header_matches(&current)
    {
        return Freshness::Stale("header drift");
    }
    match sidecar.smoke() {
        Some(s) if s.failure_detail().is_some() => return Freshness::Stale("failed smoke test"),
        None => return Freshness::Stale("no smoke record"),
        Some(_) => {}
    }
    if sidecar
        .protocol_version()
        .is_some_and(|version| version != lean_rs_worker_protocol::protocol::PROTOCOL_VERSION)
    {
        return Freshness::Stale("protocol mismatch");
    }
    if !sidecar.built_by_this_host() {
        return Freshness::Stale("built by another lean-host-mcp release");
    }
    Freshness::Current
}

/// `--clean`: remove the worker install dirs under `root` — all of them, or
/// just `target`. Idempotent.
fn clean_in(root: &Path, target: Option<&ToolchainId>) -> anyhow::Result<()> {
    if !root.is_dir() {
        println!("(no workers installed under {})", root.display());
        return Ok(());
    }
    if let Some(id) = target {
        let dir = root.join(id.as_str());
        if dir.is_dir() {
            retire_slot(&dir, &WorkerBinary::selected_dir(&dir)?)?;
            println!("removed worker for {id}");
        } else {
            println!("no worker installed for {id}");
        }
        return Ok(());
    }
    let mut removed = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_owned();
        retire_slot(&path, &WorkerBinary::selected_dir(&path)?)?;
        println!("removed worker for {name}");
        removed = removed.saturating_add(1);
    }
    println!("removed {removed} worker(s) from {}", root.display());
    Ok(())
}

// A concurrent fork can inherit the open file description until exec, even
// with close-on-exec set. End these local locks explicitly when the guard drops.
struct InstallationLock(fs::File);

impl InstallationLock {
    fn acquire(file: fs::File) -> std::io::Result<Self> {
        file.try_lock().map_err(std::io::Error::from)?;
        Ok(Self(file))
    }
}

impl Drop for InstallationLock {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "failed to unlock worker installation");
        }
    }
}

/// Serialize candidate construction, activation and retirement on the slot directory itself.
fn lock_slot(slot: &Path) -> anyhow::Result<InstallationLock> {
    let guard = InstallationLock::acquire(fs::File::open(slot)?)?;
    verify_named_handle(slot, &guard.0, true)?;
    Ok(guard)
}

fn verify_named_handle(path: &Path, file: &fs::File, directory: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let named = fs::symlink_metadata(path)?;
    let held = file.metadata()?;
    anyhow::ensure!(
        (if directory {
            named.is_dir()
        } else {
            named.is_file() && held.nlink() == 1
        }) && (held.dev(), held.ino()) == (named.dev(), named.ino()),
        "installation path changed: {}",
        path.display()
    );
    Ok(())
}

/// Take all reader domains before removing anything, including for superseded workers.
fn retire_slot(slot: &Path, expected: &Path) -> anyhow::Result<()> {
    let _mutation = lock_slot(slot)?;
    let mut guards = Vec::new();
    for entry in fs::read_dir(slot)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("generation-") {
            continue;
        }
        anyhow::ensure!(entry.file_type()?.is_dir(), "invalid worker generation directory");
        let dir = entry.path();
        for domain in [
            "shims/lean-rs-host-shims",
            "shims/lean-rs-interop-shims",
            "smoke-project",
        ] {
            let root = dir.join(domain);
            anyhow::ensure!(
                root.canonicalize()? == root && root.is_dir(),
                "invalid installed artifact domain"
            );
            let path = lean_toolchain::shared_writer::project_lock_path(&root)?;
            let guard = InstallationLock::acquire(fs::File::open(&path)?)?;
            verify_named_handle(&path, &guard.0, false)?;
            anyhow::ensure!(
                lean_toolchain::shared_writer::previous_writer_stopped(&guard.0)?,
                "installed artifact writer still active"
            );
            guards.push(guard);
        }
    }
    anyhow::ensure!(
        WorkerBinary::selected_dir(slot)? == expected,
        "selected generation changed before retirement"
    );
    fs::remove_dir_all(slot)?;
    drop(guards);
    Ok(())
}

/// `--prune`: remove only *unservable* worker install dirs under `root`,
/// leaving servable ones (even if stale) in place. Idempotent.
fn prune_in(root: &Path) -> anyhow::Result<()> {
    if !root.is_dir() {
        println!("(no workers installed under {})", root.display());
        return Ok(());
    }
    let mut removed = 0usize;
    let mut kept = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only consider real worker installs (a binary present); skip strays.
        let selected = WorkerBinary::selected_dir(&path)?;
        if !selected.join(WORKER_FILE_NAME).is_file() {
            continue;
        }
        if let Some(reason) = prune_reason(name, &selected) {
            retire_slot(&path, &selected)?;
            println!("pruned {name}: {reason}");
            removed = removed.saturating_add(1);
        } else {
            kept = kept.saturating_add(1);
        }
    }
    println!("pruned {removed} unservable worker(s); kept {kept}");
    Ok(())
}

/// Why an installed worker is unservable, or `None` if it can be served. Only
/// the two definitively-dead classes: a toolchain outside the supported window
/// (can never load), and a recorded smoke-test failure (loads but crashes).
/// Servable-but-stale workers (header drift, host skew) are deliberately kept —
/// `--auto` rebuilds those; pruning them would delete a possibly-working binary.
fn prune_reason(name: &str, install_dir: &Path) -> Option<&'static str> {
    if let Ok(id) = ToolchainId::parse(name)
        && matches!(id.window_verdict(), WindowVerdict::OutOfWindow { .. })
    {
        return Some("outside the supported window");
    }
    if let Some(sidecar) = WorkerSidecar::load(install_dir)
        && sidecar.smoke().is_some_and(|s| s.failure_detail().is_some())
    {
        return Some("failed runtime smoke test");
    }
    None
}

/// Build, qualify and select a worker for `id` in the worker directory `root`.
/// `writer`, a host executable that serves the SDK helper subcommands, builds
/// the worker's shims.
pub(super) fn install_one(
    root: &Path,
    id: &ToolchainId,
    source: &WorkerSource,
    writer: &Path,
) -> anyhow::Result<PathBuf> {
    // Classify against the supported window *before* the multi-minute build —
    // an out-of-window worker would only fail lean-rs-sys's header-digest
    // check minutes later.
    match id.window_verdict() {
        WindowVerdict::OutOfWindow { window, nearest } => {
            return Err(anyhow::anyhow!(
                "{id} is outside the lean-rs supported window {window}; nearest supported: {nearest}. \
                 Refusing to build an unsupported worker — pin a supported toolchain, or bump lean-rs first."
            ));
        }
        WindowVerdict::Unknown => {
            eprintln!(
                "warning: {id} is not a recognized lean-rs supported version (e.g. a nightly); \
                 building anyway, but the resulting worker is unsupported and may fail to load."
            );
        }
        WindowVerdict::Supported => {}
    }

    // Sanity: the elan toolchain has to exist before we ask the worker
    // crate's build.rs to point at it.
    let elan_dir = id.elan_dir()?;

    let dest_dir = root.join(id.as_str());
    let writer = writer
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot find the SDK writer {}: {e}", writer.display()))?;
    fs::create_dir_all(&dest_dir)?;
    let _mutation = lock_slot(&dest_dir)?;
    let generation = prepare_generation(&dest_dir, id, &elan_dir.canonicalize()?, &writer, |dir| {
        build_worker(source, id, &elan_dir, dir)
    })?;
    select_generation(&dest_dir, &generation)?;
    Ok(generation.join(WORKER_FILE_NAME))
}

pub(super) fn prepare_generation(
    slot: &Path,
    id: &ToolchainId,
    sysroot: &Path,
    writer: &Path,
    build: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(slot)?;
    let generation = tempfile::Builder::new().prefix("generation-").tempdir_in(slot)?;
    build(generation.path())?;
    // The generated manifests contain absolute paths, including during the smoke.
    // Keep the generation if a native operation leaves cleanup uncertain.
    let generation = generation.keep().canonicalize()?;
    super::installed_shims::qualify(&generation, id, sysroot, writer)?;
    Ok(generation)
}

fn select_generation(slot: &Path, generation: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    anyhow::ensure!(
        generation.parent() == Some(slot.canonicalize()?.as_path()),
        "generation outside install slot"
    );
    let name = generation
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid generation"))?;
    anyhow::ensure!(
        name.strip_prefix("generation-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())),
        "invalid generation name"
    );
    let mut file = tempfile::NamedTempFile::new_in(slot)?;
    file.write_all(name.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(slot.join("current-generation"))?;
    fs::File::open(slot)?.sync_all()?;
    Ok(())
}

fn discover_elan_toolchains() -> anyhow::Result<Vec<ToolchainId>> {
    let Some(home) = toolchain::elan_home() else {
        return Ok(Vec::new());
    };
    let dir = home.join("toolchains");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.ends_with("-old") || name.contains(".lock") {
            continue;
        }
        let Some(short) = name.strip_prefix("leanprover--lean4---") else {
            continue;
        };
        if let Ok(id) = ToolchainId::parse(short) {
            out.push(id);
        }
    }
    out.sort_by_key(ToolchainId::sort_key);
    Ok(out)
}

/// Where the worker crate's source comes from for a build. Resolved once per
/// run; the install path never learns which arm produced the binary.
pub(super) enum WorkerSource {
    /// A local checkout: build the worker in place, reusing cargo's incremental
    /// cache. Carries the workspace root.
    LocalWorkspace(PathBuf),
    /// No checkout: fetch and build the published `lean-host-mcp-worker` crate
    /// at this binary's own version.
    Registry,
}

/// Decide where to get the worker source. `--source-dir` wins; otherwise, if
/// this binary was built from a checkout (the worker crate is present beside
/// it), build locally; anything else (a registry-installed binary) uses the
/// published crate. A missing checkout is not an error — it is the registry
/// path.
pub(super) fn resolve_worker_source(explicit: Option<&Path>) -> anyhow::Result<WorkerSource> {
    if let Some(p) = explicit {
        if !p.is_dir() {
            return Err(anyhow::anyhow!(
                "--source-dir {} does not exist or is not a directory",
                p.display()
            ));
        }
        return Ok(WorkerSource::LocalWorkspace(p.to_path_buf()));
    }
    // `CARGO_MANIFEST_DIR` is `<workspace>/crates/lean-host-mcp` when this binary
    // was built from a checkout; for a registry-installed binary it points into
    // `~/.cargo/registry/...` with no worker crate beside it. Probe for the
    // worker crate's manifest specifically, not just any `Cargo.toml`.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    if let Some(ws) = workspace
        && ws
            .join("crates")
            .join("lean-host-mcp-worker")
            .join("Cargo.toml")
            .is_file()
    {
        return Ok(WorkerSource::LocalWorkspace(ws));
    }
    Ok(WorkerSource::Registry)
}

/// Resolve the local build output using the same working directory as Cargo.
/// An explicit empty value is rejected before starting a build.
fn local_worker_binary(workspace: &Path, target_dir: Option<&OsStr>) -> anyhow::Result<PathBuf> {
    let target = match target_dir {
        None => workspace.join("target"),
        Some(path) if path.is_empty() => return Err(anyhow::anyhow!("CARGO_TARGET_DIR must not be empty")),
        Some(path) => workspace.join(path),
    };
    Ok(target.join("release").join(WORKER_FILE_NAME))
}

fn worker_cargo(id: &ToolchainId, sysroot: &Path) -> Command {
    let mut cargo = Command::new("cargo");
    cargo
        .env("LEAN_HOST_MCP_TARGET_TOOLCHAIN", id.as_str())
        .env("LEAN_SYSROOT", sysroot);
    toolchain::pass_elan_home(&mut cargo);
    cargo
}

/// Build a worker for the selected toolchain and place it in the candidate generation.
fn build_worker(source: &WorkerSource, id: &ToolchainId, sysroot: &Path, candidate: &Path) -> anyhow::Result<()> {
    match source {
        WorkerSource::LocalWorkspace(workspace) => {
            let target_dir = std::env::var_os("CARGO_TARGET_DIR");
            let built = local_worker_binary(workspace, target_dir.as_deref())?;
            println!("==> building lean-host-mcp-worker for {id} (workspace source)");
            let status = worker_cargo(id, sysroot)
                .arg("build")
                .arg("--release")
                .arg("-p")
                .arg(WORKER_FILE_NAME)
                .current_dir(workspace)
                .status()?;
            if !status.success() {
                return Err(anyhow::anyhow!(
                    "cargo build -p lean-host-mcp-worker (toolchain {id}) failed with status {status}"
                ));
            }
            if !built.is_file() {
                return Err(anyhow::anyhow!(
                    "expected worker binary at {} but did not find one",
                    built.display()
                ));
            }
            fs::copy(built, candidate.join(WORKER_FILE_NAME))?;
        }
        WorkerSource::Registry => {
            // Pin the worker to this binary's exact version — they share the
            // workspace version and are ABI-coupled, so lockstep is intended.
            let version = env!("CARGO_PKG_VERSION");
            println!("==> installing lean-host-mcp-worker {version} for {id} (crates.io)");
            stage_registry_worker(candidate, |install_root| {
                let status = worker_cargo(id, sysroot)
                    .arg("install")
                    .arg(WORKER_FILE_NAME)
                    .arg("--version")
                    .arg(format!("={version}"))
                    .arg("--bin")
                    .arg(WORKER_FILE_NAME)
                    .arg("--root")
                    .arg(install_root)
                    .arg("--no-track")
                    .arg("--locked")
                    .status()?;
                anyhow::ensure!(
                    status.success(),
                    "cargo install lean-host-mcp-worker@={version} (toolchain {id}) failed with status {status}; \
                     a Rust toolchain and network access are required"
                );
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn stage_registry_worker(candidate: &Path, install: impl FnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let slot = candidate
        .parent()
        .ok_or_else(|| anyhow::anyhow!("candidate has no install slot"))?;
    // Cargo can leave tracking files with --no-track. Keep its install root
    // beside the generation so only the worker crosses into it, by rename.
    let scratch = tempfile::Builder::new().prefix("cargo-install-").tempdir_in(slot)?;
    install(scratch.path())?;
    let bin = scratch.path().join("bin");
    let worker = bin.join(WORKER_FILE_NAME);
    anyhow::ensure!(
        fs::symlink_metadata(&bin).is_ok_and(|meta| meta.is_dir())
            && fs::symlink_metadata(&worker).is_ok_and(|meta| meta.is_file()),
        "cargo install did not produce a regular worker binary at {}",
        worker.display()
    );
    fs::rename(worker, candidate.join(WORKER_FILE_NAME))?;
    scratch.close()?;
    Ok(())
}

pub(crate) fn sha256_prefix(path: &Path, hex_chars: usize) -> anyhow::Result<String> {
    Ok(crate::digest::file_sha256_hex(path)?.chars().take(hex_chars).collect())
}

/// Format a byte count as MiB with two decimals for the `--list` `size`
/// column. Binary file sizes are conventionally base-1024, so MiB (not MB) is
/// the natural unit; a worker binary is a few MiB. Pure integer math — whole
/// MiB, then the remainder scaled to hundredths — so there is no float cast.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "bounded base-1024 size math on a u64 byte count; `remainder * 100` \
              peaks near 1.05e8 and a worker is a few MiB, both nowhere near \
              u64 overflow"
)]
fn format_mib(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    let whole = bytes / MIB;
    let hundredths = (bytes % MIB) * 100 / MIB;
    format!("{whole}.{hundredths:02} MiB")
}

/// Tiny inline RFC 3339 (UTC) time formatter, so `install-worker --list` shows
/// `2026-05-31T18:36:11Z` instead of a raw epoch count, without taking on a date
/// crate (`chrono`/`time`). The std library gives us the epoch seconds but no
/// calendar conversion, so the civil-time math is done here by hand.
mod humantime {
    use std::time::SystemTime;

    pub(super) fn format_rfc3339_seconds_or_fallback(t: SystemTime) -> String {
        match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => format_epoch_utc(d.as_secs()),
            // A pre-1970 mtime is not worth a second algorithm for the negative
            // case; it never arises for a freshly-built worker binary.
            Err(_) => "before-epoch".into(),
        }
    }

    /// Format Unix epoch `secs` as an RFC 3339 UTC timestamp
    /// (`YYYY-MM-DDThh:mm:ssZ`).
    ///
    /// The date half is Howard Hinnant's branchless `civil_from_days`
    /// (<http://howardhinnant.github.io/date_algorithms.html#civil_from_days>):
    /// days since 1970-01-01 → `(year, month, day)`, exact for all inputs, with
    /// the era arithmetic placing the leap-day at the end of the 400-year cycle.
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        reason = "pure bounded civil-time arithmetic on a u64 epoch-seconds value; \
                  for any real file mtime every intermediate stays far inside i64 \
                  range (a year > ~2.9e11 would be needed to overflow), so neither \
                  overflow nor the single days-to-i64 cast can lose information"
    )]
    fn format_epoch_utc(secs: u64) -> String {
        let days = (secs / 86_400) as i64;
        let tod = secs % 86_400;
        let (hour, minute, second) = (tod / 3_600, (tod % 3_600) / 60, tod % 60);

        // civil_from_days: shift the epoch to 0000-03-01 so leap days fall last.
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097; // day-of-era, [0, 146096]
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year (Mar-based), [0, 365]
        let mp = (5 * doy + 2) / 153; // month, Mar=0 .. Feb=11
        let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let year = if month <= 2 { year + 1 } else { year };

        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    }

    #[cfg(test)]
    mod tests {
        use super::format_epoch_utc;

        #[test]
        fn known_epochs_render_as_rfc3339_utc() {
            assert_eq!(format_epoch_utc(0), "1970-01-01T00:00:00Z");
            // A leap-year date past Feb (exercises the Mar-based month shift).
            assert_eq!(format_epoch_utc(1_583_020_800), "2020-03-01T00:00:00Z");
            // 2026-05-31T18:36:11Z — the kind of mtime `--list` prints.
            assert_eq!(format_epoch_utc(1_780_252_571), "2026-05-31T18:36:11Z");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        reason = "tests assert the branch under test directly"
    )]

    use clap::{Args as _, Command, FromArgMatches};

    use super::{
        Freshness, InstallWorkerArgs, WorkerSource, check_arg_combination, clean_in, format_mib, local_worker_binary,
        prune_in, prune_reason, resolve_worker_source, worker_freshness_in,
    };
    use crate::toolchain::{ToolchainId, WORKER_FILE_NAME, WorkerSidecar};

    /// Create a fake worker install dir `<root>/<id>/` with a binary stub and a
    /// sidecar recorded against `digest`, for the clean/prune tests.
    fn fake_worker(root: &std::path::Path, id: &str, digest: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("binary stub");
        let tid = ToolchainId::parse(id).expect("parse id");
        WorkerSidecar::record(&dir, &tid, digest.to_owned(), crate::smoke::SmokeOutcome::Passed).expect("sidecar");
    }

    #[test]
    fn failed_upgrade_preserves_installed_worker_and_sidecar() -> anyhow::Result<()> {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let id = ToolchainId::parse("v4.36.0")?;
        fake_worker(root.path(), id.as_str(), "old");
        let slot = root.path().join(id.as_str());
        let sidecar = std::fs::read(slot.join("worker.json"))?;
        for fail_build in [true, false] {
            let failed = super::prepare_generation(&slot, &id, root.path(), root.path(), |dir| {
                anyhow::ensure!(!fail_build, "build failed");
                std::fs::write(dir.join(WORKER_FILE_NAME), b"candidate")?;
                Ok(())
            });
            assert!(failed.is_err());
            assert_eq!(crate::toolchain::WorkerBinary::selected_dir(&slot)?, slot);
            assert_eq!(std::fs::read(slot.join(WORKER_FILE_NAME))?, b"#!/bin/sh\n");
            assert_eq!(std::fs::read(slot.join("worker.json"))?, sidecar);
        }
        Ok(())
    }

    #[test]
    fn registry_install_stages_only_the_worker() -> anyhow::Result<()> {
        use std::fs;

        let slot = tempfile::Builder::new()
            .prefix("registry-install-test.")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let candidate = slot.path().join("generation-candidate");
        fs::create_dir(&candidate)?;
        let worker = b"compiled worker";
        super::stage_registry_worker(&candidate, |root| {
            fs::write(root.join(".crates.toml"), [])?;
            fs::write(root.join(".crates2.json"), [])?;
            fs::write(root.join(WORKER_FILE_NAME), b"not the bin entry")?;
            let bin = root.join("bin");
            fs::create_dir(&bin)?;
            fs::write(bin.join(WORKER_FILE_NAME), worker)?;
            Ok(())
        })?;
        assert_eq!(fs::read_dir(&candidate)?.count(), 1);
        assert_eq!(fs::read(candidate.join(WORKER_FILE_NAME))?, worker);
        assert_eq!(fs::read_dir(slot.path())?.count(), 1, "Cargo scratch was not removed");
        Ok(())
    }

    #[test]
    fn candidate_worker_replaced_after_probe_is_not_published() -> anyhow::Result<()> {
        let id = ToolchainId::parse("v4.36.0")?;
        let mut record = WorkerSidecar::candidate(&id, "header".into(), Some("before".into()));
        let error = record
            .complete("after", crate::smoke::SmokeOutcome::Passed)
            .expect_err("replacement refused");
        assert!(error.to_string().contains("candidate worker changed"));
        assert!(record.smoke().is_none());
        Ok(())
    }

    #[test]
    fn generation_selection_keeps_paths_stable_and_refuses_bad_current() -> anyhow::Result<()> {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let root = temp.path().canonicalize()?;
        let id = ToolchainId::parse("v4.36.0")?;
        fake_worker(&root, id.as_str(), "old");
        let slot = root.join(id.as_str());
        let resolve = || crate::toolchain::WorkerBinary::resolve_in(&id, &root);
        let old = resolve()?.path;
        let generation = slot.join("generation-new");
        std::fs::create_dir(&generation)?;
        std::fs::write(generation.join(WORKER_FILE_NAME), b"qualified")?;
        assert_eq!(resolve()?.path, old);
        super::select_generation(&slot, &generation)?;
        assert_eq!(resolve()?.path, generation.join(WORKER_FILE_NAME));
        assert!(old.is_file());
        std::fs::write(slot.join("current-generation"), "../outside")?;
        assert!(resolve().is_err());
        Ok(())
    }

    #[test]
    fn installation_lock_releases_with_an_inherited_descriptor() -> anyhow::Result<()> {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let path = temp.path().join("lock");
        let guard = super::InstallationLock::acquire(std::fs::File::create_new(&path)?)?;
        // Like a forked child's descriptor, this shares the open file description.
        let inherited = guard.0.try_clone()?;
        let reader = std::fs::File::open(&path)?;
        assert!(matches!(
            reader.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(guard);
        reader.try_lock_shared().map_err(std::io::Error::from)?;
        reader.unlock()?;
        drop(inherited);
        Ok(())
    }

    #[test]
    fn active_generation_refuses_retirement_until_reader_closes() -> anyhow::Result<()> {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let root = temp.path().canonicalize()?;
        let id = ToolchainId::parse("v4.23.0")?;
        let slot = root.join(id.as_str());
        std::fs::create_dir(&slot)?;
        let mut locks = Vec::new();
        for name in ["generation-old", "generation-current"] {
            let dir = slot.join(name);
            for domain in [
                "shims/lean-rs-host-shims",
                "shims/lean-rs-interop-shims",
                "smoke-project",
            ] {
                let path = dir.join(domain);
                std::fs::create_dir_all(&path)?;
                locks.push(std::fs::File::create_new(
                    lean_toolchain::shared_writer::project_lock_path(&path)?,
                )?);
            }
            std::fs::write(dir.join(WORKER_FILE_NAME), b"worker")?;
            super::select_generation(&slot, &dir)?;
        }
        let selected = slot.join("generation-current");
        assert!(super::retire_slot(&slot, &slot.join("generation-old")).is_err());
        for reader in &locks {
            reader.try_lock_shared().map_err(std::io::Error::from)?;
            assert!(super::clean_in(&root, Some(&id)).is_err());
            assert!(super::prune_in(&root).is_err());
            assert_eq!(crate::toolchain::WorkerBinary::selected_dir(&slot)?, selected);
            assert!(slot.join("generation-old").join(WORKER_FILE_NAME).is_file());
            reader.unlock()?;
        }
        super::prune_in(&root)?;
        assert!(!slot.exists());
        Ok(())
    }

    #[test]
    fn worker_cargo_sets_selected_prefix_and_toolchain() {
        use std::{ffi::OsStr, path::Path};

        let id = ToolchainId::parse("v4.36.0").expect("toolchain");
        let prefix = Path::new("/private/selected-lean");
        let cargo = super::worker_cargo(&id, prefix);
        let env: std::collections::HashMap<_, _> = cargo.get_envs().collect();
        assert_eq!(env.get(OsStr::new("LEAN_SYSROOT")), Some(&Some(prefix.as_os_str())));
        assert_eq!(
            env.get(OsStr::new("LEAN_HOST_MCP_TARGET_TOOLCHAIN")),
            Some(&Some(OsStr::new("v4.36.0")))
        );
        assert_eq!(
            env.get(OsStr::new("ELAN_HOME"))
                .copied()
                .flatten()
                .map(std::path::PathBuf::from),
            crate::toolchain::elan_home(),
            "the build finds the same elan whatever its HOME"
        );
    }

    #[test]
    fn format_mib_renders_two_decimals_base_1024() {
        assert_eq!(format_mib(0), "0.00 MiB");
        assert_eq!(format_mib(1024 * 1024), "1.00 MiB");
        // The actual worker-binary sizes the `--list` table prints.
        assert_eq!(format_mib(2_340_832), "2.23 MiB");
        assert_eq!(format_mib(3_652_448), "3.48 MiB");
    }

    fn parse(args: &[&str]) -> Result<InstallWorkerArgs, clap::Error> {
        let matches = InstallWorkerArgs::augment_args(Command::new("install-worker")).try_get_matches_from(args)?;
        InstallWorkerArgs::from_arg_matches(&matches)
    }

    #[test]
    fn no_mode_flag_parses_as_default_auto_mode() {
        let args = parse(&["install-worker"]).expect("no mode flag should parse");

        assert!(args.toolchain.is_none());
        assert!(!args.auto);
        assert!(!args.list);
    }

    #[test]
    fn source_dir_without_mode_still_parses() {
        let args = parse(&["install-worker", "--source-dir", "."]).expect("source dir only should parse");

        assert!(args.toolchain.is_none());
        assert_eq!(args.source_dir.as_deref(), Some(std::path::Path::new(".")));
    }

    #[test]
    fn mode_flags_remain_mutually_exclusive() {
        let err = parse(&["install-worker", "--auto", "--list"]).expect_err("mode flags conflict");

        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn local_worker_binary_defaults_to_the_workspace_target() {
        let workspace = std::path::Path::new("/workspace");
        assert_eq!(
            local_worker_binary(workspace, None).expect("default target"),
            workspace.join("target/release").join(WORKER_FILE_NAME)
        );
    }

    #[test]
    fn local_worker_binary_honors_an_absolute_external_target() {
        assert_eq!(
            local_worker_binary(
                std::path::Path::new("/workspace"),
                Some(std::ffi::OsStr::new("/tmp/shared-target")),
            )
            .expect("absolute target"),
            std::path::Path::new("/tmp/shared-target/release").join(WORKER_FILE_NAME)
        );
    }

    #[test]
    fn local_worker_binary_resolves_a_relative_target_against_the_cargo_workspace() {
        assert_eq!(
            local_worker_binary(
                std::path::Path::new("/workspace"),
                Some(std::ffi::OsStr::new("../shared-target")),
            )
            .expect("workspace-relative target"),
            std::path::Path::new("/workspace/../shared-target/release").join(WORKER_FILE_NAME)
        );
    }

    #[test]
    fn local_worker_binary_rejects_an_explicit_empty_target() {
        let error = local_worker_binary(std::path::Path::new("/workspace"), Some(std::ffi::OsStr::new("")))
            .expect_err("an empty override must not select a stale or unintended binary");
        assert!(error.to_string().contains("CARGO_TARGET_DIR must not be empty"));
    }

    #[test]
    fn explicit_source_dir_selects_local_workspace() {
        let dir = std::env::temp_dir();
        match resolve_worker_source(Some(&dir)).expect("existing dir resolves") {
            WorkerSource::LocalWorkspace(p) => assert_eq!(p, dir),
            WorkerSource::Registry => panic!("explicit --source-dir should select a local build"),
        }
    }

    #[test]
    fn explicit_missing_source_dir_is_an_error() {
        let missing = std::env::temp_dir().join("lhm-install-worker-no-such-dir");
        assert!(resolve_worker_source(Some(&missing)).is_err());
    }

    #[test]
    fn no_flag_inside_a_checkout_selects_local_workspace() {
        // The test binary is built from the workspace, so `CARGO_MANIFEST_DIR`'s
        // grandparent holds `crates/lean-host-mcp-worker/Cargo.toml`. (A
        // registry-installed binary, with no worker crate beside it, would
        // resolve to `Registry` — exercised by the install rehearsal, not here.)
        match resolve_worker_source(None).expect("resolves") {
            WorkerSource::LocalWorkspace(ws) => {
                assert!(
                    ws.join("crates")
                        .join("lean-host-mcp-worker")
                        .join("Cargo.toml")
                        .is_file()
                );
            }
            WorkerSource::Registry => panic!("a checkout build should select the local workspace"),
        }
    }

    #[test]
    fn clean_prune_force_flags_parse() {
        let clean = parse(&["install-worker", "--clean"]).expect("--clean parses");
        assert!(clean.clean && !clean.prune);

        let prune = parse(&["install-worker", "--prune"]).expect("--prune parses");
        assert!(prune.prune && !prune.clean);

        let forced = parse(&["install-worker", "--auto", "--force"]).expect("--auto --force parses");
        assert!(forced.auto && forced.force);
    }

    #[test]
    fn clean_with_toolchain_parses_for_targeted_removal() {
        let args = parse(&["install-worker", "--clean", "--toolchain", "v4.30.0"]).expect("--clean --toolchain parses");
        assert!(args.clean);
        assert_eq!(args.toolchain.as_deref(), Some("v4.30.0"));
    }

    #[test]
    fn action_flags_remain_mutually_exclusive() {
        // The new actions join --auto/--list in the exclusive group.
        for pair in [["--clean", "--auto"], ["--prune", "--list"], ["--clean", "--prune"]] {
            let err = parse(&["install-worker", pair[0], pair[1]]).expect_err("conflicting actions");
            assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict, "{pair:?}");
        }
    }

    #[test]
    fn toolchain_rejected_with_auto_list_prune() {
        // `--toolchain` is a target modifier validated outside clap.
        for action in ["--auto", "--list", "--prune"] {
            let args = parse(&["install-worker", "--toolchain", "v4.30.0", action]).expect("parses; rejected in run()");
            assert!(
                check_arg_combination(&args).is_err(),
                "--toolchain with {action} should be rejected"
            );
        }
    }

    #[test]
    fn toolchain_allowed_for_install_and_clean() {
        let install = parse(&["install-worker", "--toolchain", "v4.30.0"]).expect("install one");
        check_arg_combination(&install).expect("--toolchain alone is a single-toolchain install");

        let clean_one = parse(&["install-worker", "--clean", "--toolchain", "v4.30.0"]).expect("clean one");
        check_arg_combination(&clean_one).expect("--clean --toolchain removes one");
    }

    #[test]
    fn prune_targets_only_definitively_unservable_workers() {
        // Out-of-window: prunable regardless of any sidecar.
        let tmp = std::env::temp_dir().join("lhm-prune-no-such-dir");
        assert_eq!(
            prune_reason("v4.23.0", &tmp),
            Some("outside the supported window"),
            "a toolchain below the supported window is unservable"
        );
        // Unparseable directory name: not a recognized toolchain, no sidecar —
        // keep it, never prune unknown content.
        assert_eq!(
            prune_reason("not-a-toolchain", &tmp),
            None,
            "unrecognized / servable workers are kept"
        );
    }

    #[test]
    fn clean_all_removes_every_worker_dir() {
        let root = tempfile::tempdir().expect("tmp root");
        fake_worker(root.path(), "v4.30.0", "d1");
        fake_worker(root.path(), "v4.31.0-rc1", "d2");
        clean_in(root.path(), None).expect("clean all");
        assert!(!root.path().join("v4.30.0").exists());
        assert!(!root.path().join("v4.31.0-rc1").exists());
    }

    #[test]
    fn clean_one_removes_only_the_target() {
        let root = tempfile::tempdir().expect("tmp root");
        fake_worker(root.path(), "v4.30.0", "d1");
        fake_worker(root.path(), "v4.31.0-rc1", "d2");
        let target = ToolchainId::parse("v4.30.0").expect("parse");
        clean_in(root.path(), Some(&target)).expect("clean one");
        assert!(!root.path().join("v4.30.0").exists(), "target removed");
        assert!(root.path().join("v4.31.0-rc1").exists(), "others untouched");
    }

    #[test]
    fn clean_is_idempotent_on_absent_root() {
        let missing = std::env::temp_dir().join("lhm-clean-no-such-root-xyz");
        clean_in(&missing, None).expect("clean of a missing root is success");
    }

    #[test]
    fn prune_removes_unservable_keeps_servable() {
        let root = tempfile::tempdir().expect("tmp root");
        // Out-of-window → pruned (never loadable).
        fake_worker(root.path(), "v4.23.0", "d");
        // Supported + passing smoke → kept.
        fake_worker(root.path(), "v4.30.0", "d");
        // Supported but smoke FAILED → pruned (loads but crashes). Must be an
        // in-window toolchain — an out-of-window id would be pruned regardless,
        // which would let a smoke-failure regression pass silently.
        let failed_dir = root.path().join("v4.31.0");
        std::fs::create_dir_all(&failed_dir).expect("mkdir");
        std::fs::write(failed_dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        WorkerSidecar::record(
            &failed_dir,
            &ToolchainId::parse("v4.31.0").expect("parse"),
            "d".to_owned(),
            crate::smoke::SmokeOutcome::Failed {
                detail: "signal: 11 (SIGSEGV)".to_owned(),
            },
        )
        .expect("sidecar");

        prune_in(root.path()).expect("prune");
        assert!(!root.path().join("v4.23.0").exists(), "out-of-window pruned");
        assert!(!root.path().join("v4.31.0").exists(), "smoke-failed pruned");
        assert!(root.path().join("v4.30.0").exists(), "servable worker kept");
    }

    #[test]
    fn freshness_absent_when_no_binary() {
        let root = tempfile::tempdir().expect("tmp root");
        let id = ToolchainId::parse("v4.30.0").expect("parse");
        assert!(matches!(worker_freshness_in(root.path(), &id), Freshness::Absent));
    }

    #[test]
    fn freshness_current_when_built_by_this_host() {
        // `fake_worker` records via `WorkerSidecar::record`, which stamps this
        // host's version and a passing smoke. Use an id with no elan dir on this
        // machine so the header-drift check (which hashes the toolchain's real
        // lean.h) is skipped and the test isolates the host-version path.
        let root = tempfile::tempdir().expect("tmp root");
        fake_worker(root.path(), "v4.99.99", "digest");
        let id = ToolchainId::parse("v4.99.99").expect("parse");
        assert!(matches!(worker_freshness_in(root.path(), &id), Freshness::Current));
    }

    #[test]
    fn freshness_stale_without_sidecar() {
        let root = tempfile::tempdir().expect("tmp root");
        let dir = root.path().join("v4.30.0");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        let id = ToolchainId::parse("v4.30.0").expect("parse");
        assert!(matches!(worker_freshness_in(root.path(), &id), Freshness::Stale(_)));
    }

    #[test]
    fn freshness_stale_on_protocol_mismatch() {
        let root = tempfile::tempdir().expect("tmp root");
        let dir = root.path().join("v4.99.99");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        let skewed = r#"{"toolchain":"v4.99.99","header_digest":"d","built_against_lean_version":"x","built_by_host_version":"0.0.1-old","worker_protocol_version":999,"digest_supported_at_build":true,"smoke":{"result":"passed"}}"#;
        std::fs::write(dir.join("worker.json"), skewed).expect("sidecar");
        let id = ToolchainId::parse("v4.99.99").expect("parse");
        assert!(matches!(
            worker_freshness_in(root.path(), &id),
            Freshness::Stale("protocol mismatch")
        ));
    }

    #[test]
    fn freshness_stale_when_another_release_built_it() {
        // A release can keep the protocol version, as the move from lean-rs
        // 0.10 to 0.11 did, so a matching protocol is not enough.
        let root = tempfile::tempdir().expect("tmp root");
        let dir = root.path().join("v4.99.99");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        let other = format!(
            r#"{{"toolchain":"v4.99.99","header_digest":"d","built_against_lean_version":"x","built_by_host_version":"0.0.1-old","worker_protocol_version":{},"digest_supported_at_build":true,"smoke":{{"result":"passed"}}}}"#,
            lean_rs_worker_protocol::protocol::PROTOCOL_VERSION
        );
        std::fs::write(dir.join("worker.json"), other).expect("sidecar");
        let id = ToolchainId::parse("v4.99.99").expect("parse");
        assert!(matches!(
            worker_freshness_in(root.path(), &id),
            Freshness::Stale("built by another lean-host-mcp release")
        ));
    }
}
