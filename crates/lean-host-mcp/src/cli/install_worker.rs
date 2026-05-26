//! `lean-host-mcp install-worker`: build the worker binary against a
//! specific Lean toolchain and place it under
//! [`WorkerBinary::install_root`].
//!
//! Three modes:
//!
//! - `--toolchain <id>`: build for one toolchain.
//! - `--auto`: scan `~/.elan/toolchains/leanprover--lean4---*` and build
//!   for any missing ones.
//! - `--list`: print a table of currently-installed workers.
//!
//! The build shells out to `cargo build --release -p lean-host-mcp-worker`
//! with `LEAN_HOST_MCP_TARGET_TOOLCHAIN=<id>` set so the worker crate's
//! `build.rs` bakes the correct rpath. The resulting binary is moved into
//! the install root; `cargo` output streams to stdout/stderr unchanged so
//! the user sees real build errors when they occur.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use clap::Args;
use sha2::{Digest, Sha256};

use crate::toolchain::{ToolchainId, WORKER_FILE_NAME, WorkerBinary};

/// Mutually-exclusive flags for `install-worker`.
#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("mode")
        .args(["toolchain", "auto", "list"])
        .required(true),
))]
pub struct InstallWorkerArgs {
    /// Build and install for a single toolchain (e.g. `v4.30.0-rc2` or
    /// `leanprover/lean4:v4.30.0-rc2`).
    #[arg(long, value_name = "ID")]
    pub toolchain: Option<String>,

    /// Scan `~/.elan/toolchains` and install for every Lean toolchain
    /// that doesn't already have a worker.
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
/// a non-zero exit-code-equivalent error when any `--auto` install fails.
pub fn run(args: &InstallWorkerArgs) -> anyhow::Result<()> {
    if args.list {
        return run_list();
    }
    let source_dir = resolve_source_dir(args.source_dir.as_deref())?;
    if args.auto {
        run_auto(&source_dir)
    } else if let Some(raw) = args.toolchain.as_deref() {
        let id = ToolchainId::parse(raw)?;
        install_one(&id, &source_dir)?;
        Ok(())
    } else {
        // clap's ArgGroup enforces "required = true", so this is unreachable
        // in practice. Surface a clear error if the invariant is violated.
        Err(anyhow::anyhow!(
            "install-worker requires one of --toolchain, --auto, or --list"
        ))
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
        rows.push(ListRow {
            id: id.to_owned(),
            path: bin,
            size: meta.len(),
            mtime,
            sha,
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    println!("{:<28}  {:>10}  {:<24}  sha256", "toolchain", "size", "mtime");
    for row in &rows {
        let mtime = humantime::format_rfc3339_seconds_or_fallback(row.mtime);
        println!("{:<28}  {:>10}  {:<24}  {}", row.id, row.size, mtime, row.sha);
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
    size: u64,
    mtime: SystemTime,
    sha: String,
}

fn run_auto(source_dir: &Path) -> anyhow::Result<()> {
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
        match install_one(&id, source_dir) {
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

fn install_one(id: &ToolchainId, source_dir: &Path) -> anyhow::Result<PathBuf> {
    // Sanity: the elan toolchain has to exist before we ask the worker
    // crate's build.rs to point at it.
    id.elan_dir()?;

    println!("==> building lean-host-mcp-worker for {id}");
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("lean-host-mcp-worker")
        .current_dir(source_dir)
        .env("LEAN_HOST_MCP_TARGET_TOOLCHAIN", id.as_str())
        .status()?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "cargo build -p lean-host-mcp-worker (toolchain {id}) failed with status {status}"
        ));
    }

    let built = source_dir.join("target").join("release").join(WORKER_FILE_NAME);
    if !built.is_file() {
        return Err(anyhow::anyhow!(
            "expected worker binary at {} but did not find one",
            built.display()
        ));
    }

    let dest_dir = WorkerBinary::install_root().join(id.as_str());
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(WORKER_FILE_NAME);
    if dest.is_file() {
        fs::remove_file(&dest)?;
    }
    if fs::rename(&built, &dest).is_err() {
        // Cross-device move: fall back to copy + remove.
        fs::copy(&built, &dest)?;
        fs::remove_file(&built)?;
    }

    let meta = fs::metadata(&dest)?;
    let sha = sha256_prefix(&dest, 16)?;
    println!(
        "==> installed {} ({} bytes, sha256 {}…)",
        dest.display(),
        meta.len(),
        sha,
    );
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
    out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(out)
}

fn resolve_source_dir(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.is_dir() {
            return Err(anyhow::anyhow!(
                "--source-dir {} does not exist or is not a directory",
                p.display()
            ));
        }
        return Ok(p.to_path_buf());
    }
    // `CARGO_MANIFEST_DIR` for this crate is `<workspace>/crates/lean-host-mcp`.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("could not derive workspace root from CARGO_MANIFEST_DIR"))?
        .to_path_buf();
    if workspace.join("Cargo.toml").is_file() {
        return Ok(workspace);
    }
    Err(anyhow::anyhow!(
        "workspace root {} not present on disk; pass --source-dir to point at the lean-host-mcp checkout",
        workspace.display()
    ))
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

/// Tiny inline time formatter to avoid pulling in `humantime` as a new
/// dependency just for `install-worker --list`.
mod humantime {
    use std::time::SystemTime;

    pub(super) fn format_rfc3339_seconds_or_fallback(t: SystemTime) -> String {
        match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => {
                let secs = d.as_secs();
                // Civil-time conversion: `chrono` would be nicer but we
                // don't want a new dep. Show seconds-since-epoch in a
                // distinctive form so it's clear this is not a parsed
                // calendar date.
                format!("{secs}s-since-epoch")
            }
            Err(_) => "before-epoch".into(),
        }
    }
}
