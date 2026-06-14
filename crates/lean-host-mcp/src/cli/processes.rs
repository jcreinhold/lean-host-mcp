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
            parent_pid_at_start: current_parent_pid(),
            process_group_id: current_process_group_id(),
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
    #[serde(default)]
    parent_pid_at_start: Option<u32>,
    #[serde(default)]
    process_group_id: Option<u32>,
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
    current_parent_pid: Option<u32>,
    current_process_group_id: Option<u32>,
    parent_alive_at_start: Option<bool>,
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
            "pid={} alive={} executable_match={} transport={} bind={} http_path={} cwd={} started_unix_millis={} parent_pid_at_start={} current_parent_pid={} parent_alive_at_start={} process_group_id={} current_process_group_id={} stale_client={} record={}",
            listed.record.pid,
            listed.alive,
            display_match(listed.executable_match),
            listed.record.transport,
            listed.record.bind.as_deref().unwrap_or("-"),
            listed.record.http_path.as_deref().unwrap_or("-"),
            listed.record.cwd.display(),
            listed.record.started_unix_millis,
            display_u32(listed.record.parent_pid_at_start),
            display_u32(listed.current_parent_pid),
            display_bool(listed.parent_alive_at_start),
            display_u32(listed.record.process_group_id),
            display_u32(listed.current_process_group_id),
            stale_stdio_client(listed),
            listed.path.display(),
        );
        if !listed.child_pids.is_empty() {
            println!("  child_pids={}", join_u32(&listed.child_pids));
        }
        if !listed.alive {
            println!("  stale_record_cleanup=lean-host-mcp doctor processes --cleanup-stale-records");
        } else if stale_stdio_client(listed) {
            println!("  exact_terminate=kill -TERM {}", listed.record.pid);
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
        let current_parent_pid = alive.then(|| parent_pid(record.pid)).flatten();
        let current_process_group_id = alive.then(|| process_group_id(record.pid)).flatten();
        let parent_alive_at_start = record.parent_pid_at_start.map(process_alive);
        let child_pids = if alive { child_pids(record.pid) } else { Vec::new() };
        out.push(ListedRecord {
            path,
            record,
            alive,
            executable_match,
            current_parent_pid,
            current_process_group_id,
            parent_alive_at_start,
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

fn display_bool(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn display_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "-".to_owned(), |pid| pid.to_string())
}

fn stale_stdio_client(listed: &ListedRecord) -> bool {
    listed.alive
        && listed.record.transport == "stdio"
        && listed
            .record
            .parent_pid_at_start
            .is_some_and(|recorded| recorded > 1 && Some(recorded) != listed.current_parent_pid)
}

fn join_u32(values: &[u32]) -> String {
    values.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
pub fn current_parent_pid() -> Option<u32> {
    parent_pid(std::process::id())
}

#[cfg(not(unix))]
pub fn current_parent_pid() -> Option<u32> {
    None
}

#[cfg(unix)]
fn current_process_group_id() -> Option<u32> {
    process_group_id(std::process::id())
}

#[cfg(not(unix))]
fn current_process_group_id() -> Option<u32> {
    None
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
fn parent_pid(pid: u32) -> Option<u32> {
    ps_single_u32(pid, "ppid=")
}

#[cfg(not(unix))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> Option<u32> {
    ps_single_u32(pid, "pgid=")
}

#[cfg(not(unix))]
fn process_group_id(_pid: u32) -> Option<u32> {
    None
}

#[cfg(unix)]
fn ps_single_u32(pid: u32, field: &str) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
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

    #[test]
    fn doctor_stale_stdio_client_requires_exact_parent_change() {
        let listed = ListedRecord {
            path: PathBuf::from("record.json"),
            record: ProcessRecord {
                pid: 100,
                executable: PathBuf::from("/bin/lean-host-mcp"),
                cwd: PathBuf::from("/tmp/project"),
                started_unix_millis: 1,
                parent_pid_at_start: Some(50),
                process_group_id: Some(25),
                transport: "stdio".to_owned(),
                bind: None,
                http_path: None,
            },
            alive: true,
            executable_match: Some(true),
            current_parent_pid: Some(1),
            current_process_group_id: Some(25),
            parent_alive_at_start: Some(false),
            child_pids: vec![101],
        };
        assert!(stale_stdio_client(&listed));

        let mut current = listed;
        current.current_parent_pid = Some(50);
        assert!(!stale_stdio_client(&current));

        current.record.transport = "http".to_owned();
        current.current_parent_pid = Some(1);
        assert!(!stale_stdio_client(&current));
    }
}
