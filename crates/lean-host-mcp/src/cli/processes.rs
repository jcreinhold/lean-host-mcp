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
    /// Remove registry records whose PID is confirmed absent.
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
    liveness: ProcessLiveness,
    executable_match: Option<bool>,
    current_parent_pid: Option<u32>,
    current_process_group_id: Option<u32>,
    parent_liveness_at_start: Option<ProcessLiveness>,
    child_pids: Option<Vec<u32>>,
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
            "pid={} alive={} liveness_error={} executable_match={} transport={} bind={} http_path={} cwd={} started_unix_millis={} parent_pid_at_start={} current_parent_pid={} parent_alive_at_start={} process_group_id={} current_process_group_id={} stale_client={} record={}",
            listed.record.pid,
            listed.liveness.display(),
            listed.liveness.error_label(),
            display_match(listed.executable_match),
            listed.record.transport,
            listed.record.bind.as_deref().unwrap_or("-"),
            listed.record.http_path.as_deref().unwrap_or("-"),
            listed.record.cwd.display(),
            listed.record.started_unix_millis,
            display_u32(listed.record.parent_pid_at_start),
            display_u32(listed.current_parent_pid),
            listed
                .parent_liveness_at_start
                .map_or("unknown", ProcessLiveness::display),
            display_u32(listed.record.process_group_id),
            display_u32(listed.current_process_group_id),
            stale_stdio_client(listed),
            listed.path.display(),
        );
        match &listed.child_pids {
            Some(pids) => println!("  child_pids={}", join_u32(pids)),
            None => println!("  child_pids=unknown"),
        }
        if listed.liveness == ProcessLiveness::Dead {
            println!("  stale_record_cleanup=lean-host-mcp doctor processes --cleanup-stale-records");
        } else if stale_stdio_client(listed) {
            println!("  exact_terminate=kill -TERM {}", listed.record.pid);
        }
    }
    if args.cleanup_stale_records {
        cleanup_stale_records(&records, process_liveness)?;
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
        let liveness = process_liveness(record.pid);
        let observable = liveness != ProcessLiveness::Dead;
        let executable_match = observable
            .then(|| executable_matches(record.pid, &record.executable))
            .flatten();
        let current_parent_pid = observable.then(|| parent_pid(record.pid)).flatten();
        let current_process_group_id = observable.then(|| process_group_id(record.pid)).flatten();
        let parent_liveness_at_start = record.parent_pid_at_start.map(process_liveness);
        let child_pids = observable.then(|| child_pids(record.pid)).flatten();
        out.push(ListedRecord {
            path,
            record,
            liveness,
            executable_match,
            current_parent_pid,
            current_process_group_id,
            parent_liveness_at_start,
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

fn display_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "-".to_owned(), |pid| pid.to_string())
}

fn stale_stdio_client(listed: &ListedRecord) -> bool {
    listed.liveness == ProcessLiveness::Alive
        && listed.executable_match == Some(true)
        && listed.record.transport == "stdio"
        && listed
            .record
            .parent_pid_at_start
            .is_some_and(|recorded| recorded > 1 && parent_changed(recorded, listed.current_parent_pid))
}

fn join_u32(values: &[u32]) -> String {
    values.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

/// A failed observation is not evidence of death, including sandbox denials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessLiveness {
    Alive,
    Dead,
    Unknown(io::ErrorKind),
}

impl ProcessLiveness {
    fn display(self) -> &'static str {
        match self {
            Self::Alive => "true",
            Self::Dead => "false",
            Self::Unknown(_) => "unknown",
        }
    }

    fn error_label(self) -> &'static str {
        match self {
            Self::Alive | Self::Dead => "-",
            Self::Unknown(io::ErrorKind::PermissionDenied) => "permission_denied",
            Self::Unknown(io::ErrorKind::InvalidInput) => "invalid_pid",
            Self::Unknown(io::ErrorKind::Unsupported) => "unsupported",
            Self::Unknown(_) => "observation_failed",
        }
    }
}

#[cfg(unix)]
pub fn process_liveness(pid: u32) -> ProcessLiveness {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else {
        return ProcessLiveness::Unknown(io::ErrorKind::InvalidInput);
    };
    // Signal zero is a kernel existence probe, not a signal. Unlike a shell's
    // exit status, errno distinguishes ESRCH from permissions/observer failure.
    let observed = classify_liveness(rustix::process::test_kill_process(pid));
    // A sandbox may deny signal permission while still allowing another kernel
    // observation. Presence corroborates liveness; a failed fallback never
    // proves absence and does not contact the recorded server endpoint.
    corroborate_liveness(observed, || kernel_presence(pid))
}

#[cfg(target_os = "linux")]
fn kernel_presence(pid: rustix::process::Pid) -> bool {
    // Linux tasks can have process-group ID zero (e.g. kernel tasks or an
    // ancestor group outside a PID namespace). rustix's getpgid returns a
    // nonzero Pid and asserts on zero, so use procfs only as positive evidence.
    fs::metadata(format!("/proc/{}", pid.as_raw_pid())).is_ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn kernel_presence(pid: rustix::process::Pid) -> bool {
    rustix::process::getpgid(Some(pid)).is_ok()
}

#[cfg(unix)]
fn corroborate_liveness(observed: ProcessLiveness, present: impl FnOnce() -> bool) -> ProcessLiveness {
    if matches!(observed, ProcessLiveness::Unknown(_)) && present() {
        ProcessLiveness::Alive
    } else {
        observed
    }
}

#[cfg(unix)]
fn classify_liveness(result: rustix::io::Result<()>) -> ProcessLiveness {
    match result {
        Ok(()) => ProcessLiveness::Alive,
        Err(rustix::io::Errno::SRCH) => ProcessLiveness::Dead,
        Err(error) => ProcessLiveness::Unknown(io::Error::from(error).kind()),
    }
}

#[cfg(not(unix))]
pub fn process_liveness(_pid: u32) -> ProcessLiveness {
    ProcessLiveness::Unknown(io::ErrorKind::Unsupported)
}

fn parent_changed(recorded: u32, current: Option<u32>) -> bool {
    current.is_some_and(|current| current != recorded)
}

/// End a stdio session only on confirmed parent death or reparenting.
pub fn parent_lost(recorded: u32, current: Option<u32>, liveness: ProcessLiveness) -> bool {
    liveness == ProcessLiveness::Dead || parent_changed(recorded, current)
}

fn cleanup_stale_records(records: &[ListedRecord], observe: impl Fn(u32) -> ProcessLiveness) -> Result<()> {
    for listed in records {
        // Recheck before mutation; uncertainty (or a reused live PID) preserves
        // the record even when the initial observation confirmed absence.
        if listed.liveness != ProcessLiveness::Dead || observe(listed.record.pid) != ProcessLiveness::Dead {
            continue;
        }
        fs::remove_file(&listed.path).with_context(|| format!("remove stale record {}", listed.path.display()))?;
        eprintln!("removed stale process record {}", listed.path.display());
    }
    Ok(())
}

#[cfg(unix)]
pub fn current_parent_pid() -> Option<u32> {
    rustix::process::getppid().map(|pid| pid.as_raw_pid() as u32)
}

#[cfg(not(unix))]
pub fn current_parent_pid() -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn current_process_group_id() -> Option<u32> {
    process_group_id(std::process::id())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_process_group_id() -> Option<u32> {
    Some(rustix::process::getpgrp().as_raw_pid() as u32)
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
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    executable_path_matches(String::from_utf8_lossy(&output.stdout).trim(), expected)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn executable_path_matches(command: &str, expected: &Path) -> Option<bool> {
    // macOS can expose only argv[0] (e.g. `lean-host-mcp`) for PATH launches.
    // A relative label cannot identify the executable and is not a mismatch.
    // `comm` excludes arguments and preserves spaces within an absolute path.
    let actual = Path::new(command);
    actual.is_absolute().then_some(actual == expected)
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

#[cfg(target_os = "linux")]
fn process_group_id(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_group_from_stat(&stat)
}

#[cfg(any(target_os = "linux", test))]
fn process_group_from_stat(stat: &str) -> Option<u32> {
    // comm (field 2) can itself contain spaces and closing parentheses. The
    // remaining fields are scalar values, so split after the final close.
    let (_, fields) = stat.rsplit_once(") ")?;
    let group = fields.split_whitespace().nth(2)?.parse::<u32>().ok()?;
    (group > 0).then_some(group)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_group_id(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw)?;
    rustix::process::getpgid(Some(pid))
        .ok()
        .map(|pid| pid.as_raw_pid() as u32)
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
fn child_pids(parent: u32) -> Option<Vec<u32>> {
    let output = std::process::Command::new("ps").args(["-axo", "pid=,ppid="]).output();
    let output = output.ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse::<u32>().ok()?;
                let ppid = fields.next()?.parse::<u32>().ok()?;
                (ppid == parent).then_some(pid)
            })
            .collect(),
    )
}

#[cfg(not(unix))]
fn child_pids(_parent: u32) -> Option<Vec<u32>> {
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn display_match_names_unknown_and_mismatch() {
        assert_eq!(display_match(None), "unknown");
        assert_eq!(display_match(Some(false)), "no");
        assert_eq!(display_match(Some(true)), "yes");
    }

    fn sample_record() -> ListedRecord {
        ListedRecord {
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
            liveness: ProcessLiveness::Alive,
            executable_match: Some(true),
            current_parent_pid: Some(1),
            current_process_group_id: Some(25),
            parent_liveness_at_start: Some(ProcessLiveness::Dead),
            child_pids: Some(vec![101]),
        }
    }

    #[test]
    fn doctor_stale_stdio_client_requires_exact_parent_change() {
        let listed = sample_record();
        assert!(stale_stdio_client(&listed));

        let mut current = listed;
        current.current_parent_pid = Some(50);
        assert!(!stale_stdio_client(&current));

        current.record.transport = "http".to_owned();
        current.current_parent_pid = Some(1);
        assert!(!stale_stdio_client(&current));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn relative_executable_labels_are_unknown_and_spaces_are_preserved() {
        let expected = Path::new("/Applications/Lean Tools/lean-host-mcp");
        assert_eq!(executable_path_matches("lean-host-mcp", expected), None);
        assert_eq!(executable_path_matches("./lean-host-mcp", expected), None);
        assert_eq!(executable_path_matches("", expected), None);
        assert_eq!(
            executable_path_matches("/Applications/Lean Tools/lean-host-mcp", expected),
            Some(true)
        );
        assert_eq!(executable_path_matches("/other/lean-host-mcp", expected), Some(false));
    }

    #[test]
    fn doctor_never_recommends_termination_from_unknown_observations() {
        let mut listed = sample_record();
        listed.current_parent_pid = None;
        assert!(!stale_stdio_client(&listed));
        listed.current_parent_pid = Some(1);
        listed.liveness = ProcessLiveness::Unknown(io::ErrorKind::PermissionDenied);
        assert!(!stale_stdio_client(&listed));
        listed.liveness = ProcessLiveness::Alive;
        listed.executable_match = None;
        assert!(!stale_stdio_client(&listed));
        listed.executable_match = Some(false);
        assert!(!stale_stdio_client(&listed));
    }

    #[test]
    fn parent_loss_requires_confirmed_absence_or_reparenting() {
        let unknown = ProcessLiveness::Unknown(io::ErrorKind::PermissionDenied);
        assert!(!parent_lost(50, None, unknown));
        assert!(!parent_lost(50, Some(50), unknown));
        assert!(!parent_lost(50, None, ProcessLiveness::Alive));
        assert!(parent_lost(50, Some(1), unknown));
        assert!(parent_lost(50, None, ProcessLiveness::Dead));
    }

    #[cfg(unix)]
    #[test]
    fn liveness_distinguishes_absence_from_denial_and_other_errors() {
        use rustix::io::Errno;
        assert_eq!(classify_liveness(Ok(())), ProcessLiveness::Alive);
        assert_eq!(classify_liveness(Err(Errno::SRCH)), ProcessLiveness::Dead);
        for error in [Errno::PERM, Errno::ACCESS] {
            let state = classify_liveness(Err(error));
            assert_eq!(state, ProcessLiveness::Unknown(io::ErrorKind::PermissionDenied));
            assert_eq!(state.display(), "unknown");
            assert_eq!(state.error_label(), "permission_denied");
        }
        assert!(matches!(classify_liveness(Err(Errno::IO)), ProcessLiveness::Unknown(_)));
    }

    #[cfg(unix)]
    #[test]
    fn another_kernel_observation_can_confirm_presence_but_not_absence() {
        let denied = ProcessLiveness::Unknown(io::ErrorKind::PermissionDenied);
        assert_eq!(corroborate_liveness(denied, || true), ProcessLiveness::Alive);
        assert_eq!(corroborate_liveness(denied, || false), denied);
        assert_eq!(
            corroborate_liveness(ProcessLiveness::Dead, || true),
            ProcessLiveness::Dead
        );
    }

    #[cfg(unix)]
    #[test]
    fn liveness_uses_the_kernel_and_rejects_non_process_pids() {
        assert_eq!(process_liveness(std::process::id()), ProcessLiveness::Alive);
        for pid in [0, u32::MAX] {
            assert_eq!(
                process_liveness(pid),
                ProcessLiveness::Unknown(io::ErrorKind::InvalidInput)
            );
        }
        assert!(current_parent_pid().is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(current_process_group_id().is_some());
        // Linux may expose no visible process group; zero is not a usable ID.
        #[cfg(target_os = "linux")]
        assert_ne!(current_process_group_id(), Some(0));
    }

    #[test]
    fn linux_process_group_parser_handles_zero_and_parentheses_without_panicking() {
        assert_eq!(process_group_from_stat("42 (worker) S 7 42 42 0 0"), Some(42));
        assert_eq!(process_group_from_stat("42 (worker ) name) S 7 23 23 0 0"), Some(23));
        for stat in [
            "42 (kernel) S 7 0 0 0 0",
            "42 (worker) S 7 -1",
            "missing",
            "42 (worker) S",
        ] {
            assert_eq!(process_group_from_stat(stat), None);
        }
    }

    #[test]
    fn cleanup_preserves_unknown_and_rechecks_before_removing() {
        let dir = tempfile::tempdir().expect("temp registry");
        let mut listed = sample_record();
        listed.path = dir.path().join("100.json");
        fs::write(&listed.path, b"record").expect("write record");
        let unknown = ProcessLiveness::Unknown(io::ErrorKind::PermissionDenied);
        for initial in [ProcessLiveness::Alive, unknown] {
            listed.liveness = initial;
            cleanup_stale_records(std::slice::from_ref(&listed), |_| ProcessLiveness::Dead).expect("cleanup");
            assert!(listed.path.exists());
        }
        listed.liveness = ProcessLiveness::Dead;
        for rechecked in [ProcessLiveness::Alive, unknown] {
            cleanup_stale_records(std::slice::from_ref(&listed), |_| rechecked).expect("cleanup");
            assert!(listed.path.exists());
        }
        cleanup_stale_records(std::slice::from_ref(&listed), |_| ProcessLiveness::Dead).expect("cleanup");
        assert!(!listed.path.exists());
    }
}
