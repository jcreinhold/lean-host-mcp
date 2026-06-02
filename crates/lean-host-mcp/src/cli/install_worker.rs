//! `lean-host-mcp install-worker`: build the worker binary against a
//! specific Lean toolchain and place it under
//! [`WorkerBinary::install_root`].
//!
//! Actions:
//!
//! - `--toolchain <id>`: build for one toolchain (always overwrites).
//! - no flag / `--auto`: scan `~/.elan/toolchains/leanprover--lean4---*`
//!   and build for any that are missing or stale (host-version skew, header
//!   drift, failed/absent smoke); `--force` rebuilds current ones too.
//! - `--list`: print a table of currently-installed workers, including a
//!   `host` column that flags workers built by a different (version-locked)
//!   `lean-host-mcp` than the one running.
//! - `--clean [--toolchain <id>]`: remove all installed workers, or just one.
//! - `--prune`: remove only unservable workers (outside the supported window,
//!   or with a failed smoke test), keeping servable-but-stale ones.
//!
//! The worker is always compiled locally per toolchain (its `build.rs` bakes an
//! absolute rpath, so binaries don't travel), with `LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>`
//! set so the right rpath is baked in. Where the worker *source* comes from is
//! decided once, internally, by [`resolve_worker_source`]: a local checkout
//! (`cargo build -p lean-host-mcp-worker`) when this binary was built from one,
//! otherwise the published crate (`cargo install lean-host-mcp-worker`). Callers
//! of `install-worker` never choose; `--source-dir` is the override for the rare
//! case of a checkout that moved after the binary was built. `cargo` output
//! streams to stdout/stderr unchanged so the user sees real build errors.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use clap::Args;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::toolchain::{ToolchainId, WORKER_FILE_NAME, WindowVerdict, WorkerBinary, WorkerSidecar, hash_lean_header};

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

    /// Scan `~/.elan/toolchains` and install a worker for every supported
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
    if args.list {
        return run_list();
    }
    if args.prune {
        return run_prune();
    }
    if args.clean {
        // `--clean --toolchain x` removes one; `--clean` alone removes all.
        let target = args.toolchain.as_deref().map(ToolchainId::parse).transpose()?;
        return run_clean(target.as_ref());
    }
    // Default action (no flag) and `--auto` both scan and install.
    let source = resolve_worker_source(args.source_dir.as_deref())?;
    if let Some(raw) = args.toolchain.as_deref() {
        // `check_arg_combination` ruled out `--toolchain --auto`, so this is a
        // single-toolchain install (always overwrites — no freshness check).
        let id = ToolchainId::parse(raw)?;
        install_one(&id, &source)?;
        return Ok(());
    }
    run_auto(&source, args.force)
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

fn run_list() -> anyhow::Result<()> {
    let root = WorkerBinary::install_root();
    if !root.is_dir() {
        println!("(no workers installed under {})", root.display());
        return Ok(());
    }
    let mut rows: Vec<ListRow> = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let id_path = entry.path();
        if !id_path.is_dir() {
            continue;
        }
        let Some(id) = id_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let bin = id_path.join(WORKER_FILE_NAME);
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
        // One sidecar load feeds both the `build` (header-drift) and `runtime`
        // (smoke) columns. No sidecar means no provenance at all, so both are
        // `unknown`/`untested` — the same labels their per-state helpers use.
        let sidecar = WorkerSidecar::load(&id_path);
        let header = sidecar
            .as_ref()
            .map_or("unknown", |s| s.header_status(current.as_deref()));
        let smoke = sidecar.as_ref().map_or("untested", WorkerSidecar::smoke_status);
        // Host-version provenance: `current` if built by this host, `stale` if
        // by a different (version-locked) host — the skew that silently served
        // an ABI-mismatched worker before this column existed.
        let host = sidecar
            .as_ref()
            .map_or("unknown", |s| s.host_status(env!("CARGO_PKG_VERSION")));
        rows.push(ListRow {
            id: id.to_owned(),
            path: bin,
            support,
            header,
            smoke,
            host,
            size: meta.len(),
            mtime,
            sha,
            sort_key,
        });
    }
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    println!(
        "{:<28}  {:<14}  {:<9}  {:<9}  {:<9}  {:>10}  {:<24}  sha256",
        "toolchain", "support", "build", "runtime", "host", "size", "built"
    );
    for row in &rows {
        let mtime = humantime::format_rfc3339_seconds_or_fallback(row.mtime);
        let size = format_mib(row.size);
        println!(
            "{:<28}  {:<14}  {:<9}  {:<9}  {:<9}  {:>10}  {:<24}  {}",
            row.id, row.support, row.header, row.smoke, row.host, size, mtime, row.sha
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
    host: &'static str,
    size: u64,
    mtime: SystemTime,
    sha: String,
    /// Semantic ordering key (rc before its release); see [`ToolchainId::sort_key`].
    sort_key: (u8, (u32, u32, u32, u8, u32), String),
}

fn run_auto(source: &WorkerSource, force: bool) -> anyhow::Result<()> {
    let toolchains = discover_elan_toolchains()?;
    if toolchains.is_empty() {
        println!("(no Lean toolchains found under ~/.elan/toolchains)");
        return Ok(());
    }
    let mut failed = false;
    for id in toolchains {
        match auto_decision(&id, force) {
            AutoDecision::SkipUnsupported => println!("{id}: skipped (outside supported window)"),
            AutoDecision::SkipCurrent => {
                println!("{id}: current; skipping (use --force to rebuild)");
            }
            AutoDecision::Install(reason) => match install_one(&id, source) {
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

fn auto_decision(id: &ToolchainId, force: bool) -> AutoDecision {
    if matches!(id.window_verdict(), WindowVerdict::OutOfWindow { .. }) {
        return AutoDecision::SkipUnsupported;
    }
    match worker_freshness(id) {
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

/// Whether the worker already installed under [`WorkerBinary::install_root`] for
/// `id` is current, stale, or absent. Mirrors the runtime open-gate's provenance
/// checks ([`WorkerBinary::resolve_ready_with_override`]) but reads the install
/// root directly — `install-worker` always writes there, regardless of the
/// `LEAN_HOST_MCP_WORKERS_DIR` override the runtime path honors.
enum Freshness {
    Absent,
    Current,
    Stale(&'static str),
}

fn worker_freshness(id: &ToolchainId) -> Freshness {
    worker_freshness_in(&WorkerBinary::install_root(), id)
}

/// Freshness against an explicit `root`. Split out of [`worker_freshness`] so
/// the staleness decision is testable against a temp root (mirroring
/// [`clean_in`] / [`prune_in`]).
fn worker_freshness_in(root: &Path, id: &ToolchainId) -> Freshness {
    let dir = root.join(id.as_str());
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
    // Worker and host are version-locked; a different builder version may speak
    // a different worker protocol. `""` means the sidecar predates the field.
    let built = sidecar.host_version();
    if !built.is_empty() && built != env!("CARGO_PKG_VERSION") {
        return Freshness::Stale("host-version skew");
    }
    Freshness::Current
}

/// `--clean`: remove installed workers — all of them, or just `target`.
/// Idempotent. Thin wrapper over [`clean_in`] with the real install root.
fn run_clean(target: Option<&ToolchainId>) -> anyhow::Result<()> {
    clean_in(&WorkerBinary::install_root(), target)
}

/// Remove worker install dirs under `root`. Pulled out of [`run_clean`] so the
/// removal can be tested against a temp root without redirecting the real one
/// (`install_root` reads no env override).
fn clean_in(root: &Path, target: Option<&ToolchainId>) -> anyhow::Result<()> {
    if !root.is_dir() {
        println!("(no workers installed under {})", root.display());
        return Ok(());
    }
    if let Some(id) = target {
        let dir = root.join(id.as_str());
        if dir.is_dir() {
            fs::remove_dir_all(&dir)?;
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
        fs::remove_dir_all(&path)?;
        println!("removed worker for {name}");
        removed = removed.saturating_add(1);
    }
    println!("removed {removed} worker(s) from {}", root.display());
    Ok(())
}

/// `--prune`: remove only *unservable* workers, leaving servable ones (even if
/// stale) in place. Idempotent. Thin wrapper over [`prune_in`].
fn run_prune() -> anyhow::Result<()> {
    prune_in(&WorkerBinary::install_root())
}

/// Remove unservable worker install dirs under `root`. Pulled out of
/// [`run_prune`] for the same testability reason as [`clean_in`].
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
        if !path.join(WORKER_FILE_NAME).is_file() {
            continue;
        }
        if let Some(reason) = prune_reason(name, &path) {
            fs::remove_dir_all(&path)?;
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

fn install_one(id: &ToolchainId, source: &WorkerSource) -> anyhow::Result<PathBuf> {
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

    // Build the worker (local checkout or published crate — `build_worker`
    // hides which). `staged` owns the temp dir for the registry build, so it
    // must stay alive until the binary has been relocated below.
    let staged = build_worker(source, id)?;

    let dest_dir = WorkerBinary::install_root().join(id.as_str());
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(WORKER_FILE_NAME);
    if dest.is_file() {
        fs::remove_file(&dest)?;
    }
    if fs::rename(&staged.binary, &dest).is_err() {
        // Cross-device move: fall back to copy + remove.
        fs::copy(&staged.binary, &dest)?;
        fs::remove_file(&staged.binary)?;
    }

    // Prove the worker can actually run before vouching for it: a matching
    // header digest does not imply ABI compatibility with this toolchain's
    // libleanshared, so spawn the binary once and run a trivial real
    // elaboration. This runs here, over the multi-minute build, instead of
    // letting every project-open rediscover a crash at call time.
    println!("==> smoke test: inspect Nat.add_zero [imports=Init] for {id}");
    let smoke = crate::smoke::probe(&dest, &elan_dir, id);

    // Record provenance next to the binary: the full lean.h digest the worker
    // was built against (so a later open can detect header drift — the rc
    // republished under the same id) and the smoke outcome (so the gate can
    // demote a worker that builds and digest-matches but cannot run).
    let header_digest = hash_lean_header(&elan_dir)?;
    let supported = lean_toolchain::supported_by_digest(&header_digest).is_some();
    WorkerSidecar::record(&dest_dir, id, header_digest, smoke.clone())?;

    let meta = fs::metadata(&dest)?;
    let sha = sha256_prefix(&dest, 16)?;
    println!(
        "==> installed {} ({} bytes, sha256 {}…, digest {}, runtime {})",
        dest.display(),
        meta.len(),
        sha,
        if supported { "supported" } else { "unrecognized" },
        smoke.label(),
    );

    // A worker that built and digest-matched but crashed the smoke test is
    // recorded as unusable (so `--list` shows it and the gate refuses it) and
    // surfaced as a hard install failure — exit non-zero so the user/CI sees it.
    if let Some(detail) = smoke.failure_detail() {
        return Err(anyhow::anyhow!(
            "worker for {id} built but FAILED its runtime smoke test ({detail}); this toolchain's \
             libleanshared is ABI-incompatible with this lean-rs build. The worker is recorded as \
             unusable (runtime=crashed) and will not be served — pin a supported toolchain the host can \
             run, or rebuild lean-rs."
        ));
    }
    Ok(dest)
}

fn discover_elan_toolchains() -> anyhow::Result<Vec<ToolchainId>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(Vec::new());
    };
    let dir = home.join(".elan").join("toolchains");
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
enum WorkerSource {
    /// A local checkout: build the worker in place, reusing cargo's incremental
    /// cache. Carries the workspace root.
    LocalWorkspace(PathBuf),
    /// No checkout: fetch and build the published `lean-host-mcp-worker` crate
    /// at this binary's own version.
    Registry,
}

/// A freshly built worker binary, ready to relocate into the install root.
struct StagedWorker {
    /// Path to the built binary.
    binary: PathBuf,
    /// Kept alive (registry build only) so the binary isn't deleted before it
    /// is relocated; dropping a [`TempDir`] removes its contents.
    _tmp: Option<TempDir>,
}

/// Decide where to get the worker source. `--source-dir` wins; otherwise, if
/// this binary was built from a checkout (the worker crate is present beside
/// it), build locally; anything else (a registry-installed binary) uses the
/// published crate. A missing checkout is not an error — it is the registry
/// path.
fn resolve_worker_source(explicit: Option<&Path>) -> anyhow::Result<WorkerSource> {
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

/// Build a worker binary for `id` with the matching toolchain rpath baked in,
/// returning its path (and, for the registry build, the temp-dir guard that
/// must outlive the relocate). This is the only place the two source strategies
/// diverge.
fn build_worker(source: &WorkerSource, id: &ToolchainId) -> anyhow::Result<StagedWorker> {
    match source {
        WorkerSource::LocalWorkspace(workspace) => {
            println!("==> building lean-host-mcp-worker for {id} (workspace source)");
            let status = Command::new("cargo")
                .arg("build")
                .arg("--release")
                .arg("-p")
                .arg(WORKER_FILE_NAME)
                .current_dir(workspace)
                .env("LEAN_HOST_MCP_TARGET_TOOLCHAIN", id.as_str())
                .status()?;
            if !status.success() {
                return Err(anyhow::anyhow!(
                    "cargo build -p lean-host-mcp-worker (toolchain {id}) failed with status {status}"
                ));
            }
            let built = workspace.join("target").join("release").join(WORKER_FILE_NAME);
            if !built.is_file() {
                return Err(anyhow::anyhow!(
                    "expected worker binary at {} but did not find one",
                    built.display()
                ));
            }
            Ok(StagedWorker {
                binary: built,
                _tmp: None,
            })
        }
        WorkerSource::Registry => {
            // Pin the worker to this binary's exact version — they share the
            // workspace version and are ABI-coupled, so lockstep is intended.
            let version = env!("CARGO_PKG_VERSION");
            println!("==> installing lean-host-mcp-worker {version} for {id} (crates.io)");
            let tmp = tempfile::tempdir()?;
            let status = Command::new("cargo")
                .arg("install")
                .arg(WORKER_FILE_NAME)
                .arg("--version")
                .arg(format!("={version}"))
                .arg("--bin")
                .arg(WORKER_FILE_NAME)
                .arg("--root")
                .arg(tmp.path())
                .arg("--locked")
                .env("LEAN_HOST_MCP_TARGET_TOOLCHAIN", id.as_str())
                .status()?;
            if !status.success() {
                return Err(anyhow::anyhow!(
                    "cargo install lean-host-mcp-worker@={version} (toolchain {id}) failed with status {status}; \
                     a Rust toolchain and network access are required"
                ));
            }
            let built = tmp.path().join("bin").join(WORKER_FILE_NAME);
            if !built.is_file() {
                return Err(anyhow::anyhow!(
                    "cargo install did not produce a worker binary at {}",
                    built.display()
                ));
            }
            Ok(StagedWorker {
                binary: built,
                _tmp: Some(tmp),
            })
        }
    }
}

fn sha256_prefix(path: &Path, hex_chars: usize) -> anyhow::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 8192];
    let mut hasher = Sha256::new();
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or(&[]));
    }
    use std::fmt::Write as _;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len().saturating_mul(2));
    for b in &digest {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex.chars().take(hex_chars).collect())
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
    #![allow(clippy::expect_used, clippy::panic)]

    use clap::{Args as _, Command, FromArgMatches};

    use super::{
        Freshness, InstallWorkerArgs, WorkerSource, check_arg_combination, clean_in, format_mib, prune_in,
        prune_reason, resolve_worker_source, worker_freshness_in,
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
        // Supported but smoke FAILED → pruned (loads but crashes).
        let failed_dir = root.path().join("v4.29.0");
        std::fs::create_dir_all(&failed_dir).expect("mkdir");
        std::fs::write(failed_dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        WorkerSidecar::record(
            &failed_dir,
            &ToolchainId::parse("v4.29.0").expect("parse"),
            "d".to_owned(),
            crate::smoke::SmokeOutcome::Failed {
                detail: "signal: 11 (SIGSEGV)".to_owned(),
            },
        )
        .expect("sidecar");

        prune_in(root.path()).expect("prune");
        assert!(!root.path().join("v4.23.0").exists(), "out-of-window pruned");
        assert!(!root.path().join("v4.29.0").exists(), "smoke-failed pruned");
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
    fn freshness_stale_on_host_version_skew() {
        // Absent-from-elan id so header drift is skipped and the *host skew* is
        // the reason; a passing smoke rules out the smoke-stale path too.
        let root = tempfile::tempdir().expect("tmp root");
        let dir = root.path().join("v4.99.99");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(WORKER_FILE_NAME), b"#!/bin/sh\n").expect("stub");
        let skewed = r#"{"toolchain":"v4.99.99","header_digest":"d","built_against_lean_version":"x","built_by_host_version":"0.0.1-old","digest_supported_at_build":true,"smoke":{"result":"passed"}}"#;
        std::fs::write(dir.join("worker.json"), skewed).expect("sidecar");
        let id = ToolchainId::parse("v4.99.99").expect("parse");
        assert!(matches!(
            worker_freshness_in(root.path(), &id),
            Freshness::Stale("host-version skew")
        ));
    }
}
