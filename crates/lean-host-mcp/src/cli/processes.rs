//! Process-registry diagnostics for `lean-host-mcp` servers.
//!
//! The registry is intentionally narrow: only a running server writes its own
//! record, and the doctor command lists or removes those records. It never
//! scans process names and never kills a PID by substring.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

const REGISTRY_ENV: &str = "LEAN_HOST_MCP_PROCESS_REGISTRY_DIR";

#[derive(Debug, Args)]
pub struct DoctorProcessesArgs {
    /// Remove registry records whose PID is no longer alive.
    #[arg(long)]
    pub cleanup_stale_records: bool,
}

#[derive(Debug)]
pub struct ServerProcessRecord {
    path: PathBuf,
}

impl ServerProcessRecord {
    /// Register this running server in the per-user process registry.
    ///
    /// The record is removed on normal process shutdown. If a launcher kills
    /// the process before `Drop` runs, `doctor processes --cleanup-stale-records`
    /// removes the stale record later.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry directory, current executable, current
    /// directory, record serialization, or record write cannot be resolved.
    pub fn register(transport: &str, bind: Option<String>, http_path: Option<&str>) -> Result<Self> {
        let dir = registry_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("create process registry {}", dir.display()))?;
        let pid = std::process::id();
        let path = dir.join(format!("{pid}.json"));
        let record = ProcessRecord {
            pid,
            executable: std::env::current_exe().context("resolve current executable")?,
            cwd: std::env::current_dir().context("resolve current working directory")?,
            started_unix_millis: unix_millis(),
            transport: transport.to_owned(),
            bind,
            http_path: http_path.map(ToOwned::to_owned),
        };
        let bytes = serde_json::to_vec_pretty(&record).context("serialize process record")?;
        fs::write(&path, bytes).with_context(|| format!("write process record {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for ServerProcessRecord {
    fn drop(&mut self) {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(path = %self.path.display(), error = %err, "remove process record failed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessRecord {
    pid: u32,
    executable: PathBuf,
    cwd: PathBuf,
    started_unix_millis: u128,
    transport: String,
    bind: Option<String>,
    http_path: Option<String>,
}

#[derive(Debug)]
struct ListedRecord {
    path: PathBuf,
    record: ProcessRecord,
    alive: bool,
    executable_match: Option<bool>,
    child_pids: Vec<u32>,
}

/// List registered server records and optionally remove stale records.
///
/// # Errors
///
/// Returns an error if the registry cannot be read, a record cannot be parsed,
/// or a requested stale-record cleanup cannot remove its file.
pub fn run(args: &DoctorProcessesArgs) -> Result<()> {
    let records = list_records()?;
    for listed in &records {
        println!(
            "pid={} alive={} executable_match={} transport={} bind={} http_path={} cwd={} record={}",
            listed.record.pid,
            listed.alive,
            display_match(listed.executable_match),
            listed.record.transport,
            listed.record.bind.as_deref().unwrap_or("-"),
            listed.record.http_path.as_deref().unwrap_or("-"),
            listed.record.cwd.display(),
            listed.path.display(),
        );
        if !listed.child_pids.is_empty() {
            println!("  child_pids={}", join_u32(&listed.child_pids));
        }
    }
    if args.cleanup_stale_records {
        for listed in records {
            if listed.alive {
                continue;
            }
            fs::remove_file(&listed.path).with_context(|| format!("remove stale record {}", listed.path.display()))?;
            eprintln!("removed stale process record {}", listed.path.display());
        }
    }
    Ok(())
}

fn list_records() -> Result<Vec<ListedRecord>> {
    let dir = registry_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("read process registry {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("read process record {}", path.display()))?;
        let record: ProcessRecord =
            serde_json::from_slice(&bytes).with_context(|| format!("parse process record {}", path.display()))?;
        let alive = process_alive(record.pid);
        let executable_match = alive
            .then(|| executable_matches(record.pid, &record.executable))
            .flatten();
        let child_pids = if alive { child_pids(record.pid) } else { Vec::new() };
        out.push(ListedRecord {
            path,
            record,
            alive,
            executable_match,
            child_pids,
        });
    }
    out.sort_by_key(|listed| listed.record.pid);
    Ok(out)
}

fn registry_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(REGISTRY_ENV) {
        return Ok(PathBuf::from(path));
    }
    let cache = dirs::cache_dir().context("could not resolve user cache directory")?;
    Ok(cache.join("lean-host-mcp").join("processes"))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn display_match(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn join_u32(values: &[u32]) -> String {
    values.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn executable_matches(pid: u32, expected: &Path) -> Option<bool> {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|actual| actual == expected)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn executable_matches(pid: u32, expected: &Path) -> Option<bool> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let actual = command.split_whitespace().next()?;
    Some(Path::new(actual) == expected)
}

#[cfg(not(unix))]
fn executable_matches(_pid: u32, _expected: &Path) -> Option<bool> {
    None
}

#[cfg(unix)]
fn child_pids(parent: u32) -> Vec<u32> {
    let output = std::process::Command::new("ps").args(["-axo", "pid=,ppid="]).output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

#[cfg(not(unix))]
fn child_pids(_parent: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_match_names_unknown_and_mismatch() {
        assert_eq!(display_match(None), "unknown");
        assert_eq!(display_match(Some(false)), "no");
        assert_eq!(display_match(Some(true)), "yes");
    }
}
