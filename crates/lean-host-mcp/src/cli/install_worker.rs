//! `lean-host-mcp install-worker`: build the worker binary against a
//! specific Lean toolchain and place it under
//! [`WorkerBinary::install_root`].
//!
//! Three modes:
//!
//! - `--toolchain <id>`: build for one toolchain.
//! - no flag / `--auto`: scan `~/.elan/toolchains/leanprover--lean4---*`
//!   and build for any missing ones.
//! - `--list`: print a table of currently-installed workers.
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

/// Mutually-exclusive flags for `install-worker`.
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("mode")
        .args(["toolchain", "auto", "list"])
))]
pub struct InstallWorkerArgs {
    /// Build and install for a single toolchain (e.g. `v4.30.0` or
    /// `leanprover/lean4:v4.30.0`).
    #[arg(long, value_name = "ID")]
    pub toolchain: Option<String>,

    /// Scan `~/.elan/toolchains` and install for every Lean toolchain
    /// that doesn't already have a worker. This is the default when no
    /// mode flag is supplied.
    #[arg(long)]
    pub auto: bool,

    /// Print a table of currently-installed worker binaries.
    #[arg(long)]
    pub list: bool,

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
    if args.list {
        return run_list();
    }
    let source = resolve_worker_source(args.source_dir.as_deref())?;
    if args.auto {
        run_auto(&source)
    } else if let Some(raw) = args.toolchain.as_deref() {
        let id = ToolchainId::parse(raw)?;
        install_one(&id, &source)?;
        Ok(())
    } else {
        run_auto(&source)
    }
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
        rows.push(ListRow {
            id: id.to_owned(),
            path: bin,
            support,
            header,
            smoke,
            size: meta.len(),
            mtime,
            sha,
            sort_key,
        });
    }
    rows.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    println!(
        "{:<28}  {:<14}  {:<9}  {:<9}  {:>10}  {:<24}  sha256",
        "toolchain", "support", "build", "runtime", "size", "built"
    );
    for row in &rows {
        let mtime = humantime::format_rfc3339_seconds_or_fallback(row.mtime);
        let size = format_mib(row.size);
        println!(
            "{:<28}  {:<14}  {:<9}  {:<9}  {:>10}  {:<24}  {}",
            row.id, row.support, row.header, row.smoke, size, mtime, row.sha
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
    size: u64,
    mtime: SystemTime,
    sha: String,
    /// Semantic ordering key (rc before its release); see [`ToolchainId::sort_key`].
    sort_key: (u8, (u32, u32, u32, u8, u32), String),
}

fn run_auto(source: &WorkerSource) -> anyhow::Result<()> {
    let toolchains = discover_elan_toolchains()?;
    if toolchains.is_empty() {
        println!("(no Lean toolchains found under ~/.elan/toolchains)");
        return Ok(());
    }
    let mut failed = false;
    for id in toolchains {
        let target = WorkerBinary::install_root().join(id.as_str()).join(WORKER_FILE_NAME);
        if target.is_file() {
            println!("{id}: already-installed");
            continue;
        }
        match install_one(&id, source) {
            Ok(_) => println!("{id}: installed"),
            Err(err) => {
                eprintln!("{id}: failed: {err}");
                failed = true;
            }
        }
    }
    if failed {
        Err(anyhow::anyhow!("one or more --auto installs failed"))
    } else {
        Ok(())
    }
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

    use super::{InstallWorkerArgs, WorkerSource, format_mib, resolve_worker_source};

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
}
