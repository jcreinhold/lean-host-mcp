//! The helper's log mode: one pipe for a stage's stdout and stderr, appended
//! to a file the server names, up to a byte cap.
//!
//! A build can print far more than the 64 KiB a stopped frame may carry, and
//! reaching a cap must not stop the build. So the helper copies the stage's
//! output into the file until the file holds `limit` bytes, writes one line
//! saying that the rest was dropped, and keeps reading the pipe so the stage
//! never blocks on it. Only a truncated log is longer than its cap.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{Mode, OFlags};
use serde_json::Value;

use super::invalid;

/// The log file for one stage, set by the server. The helper removes it from
/// the stage's environment.
pub(crate) const LOG_ENV: &str = "LEAN_HOST_MCP_LOG";
/// The log's cap in bytes, as a decimal string.
pub(crate) const LOG_LIMIT_ENV: &str = "LEAN_HOST_MCP_LOG_LIMIT";
/// Written once, after the last byte that fits under the cap.
pub(super) const TRUNCATED: &[u8] = b"\n[lean-host-mcp: log limit reached; later output dropped]\n";
/// One read from a stage's pipe. Thirty-two of them bound one poll.
const READ_BYTES: usize = 64 * 1024;

/// A log file that keeps at most `limit` bytes, then one truncation line.
pub(crate) struct CappedLog {
    file: File,
    len: u64,
    limit: u64,
}

impl CappedLog {
    /// Open `path` for appending, creating it with mode 0600. Refuses a
    /// symbolic link and anything that is not a regular file.
    pub(crate) fn open(path: &Path, limit: u64) -> io::Result<Self> {
        let file = File::from(rustix::fs::open(
            path,
            OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("writer log is not a regular file"));
        }
        Ok(Self {
            file,
            len: metadata.len(),
            limit,
        })
    }

    /// Whether output was dropped.
    pub(crate) fn truncated(&self) -> bool {
        self.len > self.limit
    }

    /// Append what fits under the cap. When bytes do not fit, write the
    /// truncation line after the part that does; drop everything later.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || self.truncated() {
            return Ok(());
        }
        let room = usize::try_from(self.limit.saturating_sub(self.len)).unwrap_or(usize::MAX);
        let (kept, dropped) = bytes
            .split_at_checked(bytes.len().min(room))
            .ok_or_else(|| invalid("log split out of range"))?;
        self.file.write_all(kept)?;
        self.len = self.len.saturating_add(kept.len() as u64);
        if !dropped.is_empty() {
            self.file.write_all(TRUNCATED)?;
            self.len = self.len.saturating_add(TRUNCATED.len() as u64);
        }
        Ok(())
    }
}

/// Where one stage's output goes.
pub(super) struct LogTarget {
    pub(super) path: PathBuf,
    pub(super) limit: u64,
}

/// Take the log settings out of a run message's environment, so the stage
/// never sees them. A message sets both or neither.
pub(super) fn take_log(message: &mut Value) -> io::Result<Option<LogTarget>> {
    let Some(env) = message.get_mut("env").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    match (env.remove(LOG_ENV), env.remove(LOG_LIMIT_ENV)) {
        (None, None) => Ok(None),
        (Some(Value::String(path)), Some(Value::String(limit))) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(invalid("writer log path must be absolute"));
            }
            let limit = limit
                .parse()
                .map_err(|_| invalid("writer log limit must be a byte count"))?;
            Ok(Some(LogTarget { path, limit }))
        }
        _ => Err(invalid(format!(
            "writer log needs both {LOG_ENV} and {LOG_LIMIT_ENV} as strings"
        ))),
    }
}

/// The helper's end of a stage's output pipe and the log it feeds.
pub(super) struct LogSink {
    descriptor: OwnedFd,
    log: CappedLog,
    buffer: Vec<u8>,
}

impl LogSink {
    pub(super) fn new(descriptor: OwnedFd, target: &LogTarget) -> io::Result<Self> {
        Ok(Self {
            descriptor,
            log: CappedLog::open(&target.path, target.limit)?,
            buffer: vec![0; READ_BYTES],
        })
    }

    /// Copy what the pipe holds now into the log. Past the cap the bytes are
    /// read and dropped, so the stage keeps running. A pipe holds at most
    /// 1 MiB, so one call empties it once every writer has exited.
    pub(super) fn read_available(&mut self) -> io::Result<()> {
        for _ in 0..32 {
            match rustix::io::read(&self.descriptor, self.buffer.as_mut_slice()) {
                Ok(0) => return Ok(()),
                Ok(count) => self.log.append(
                    self.buffer
                        .get(..count)
                        .ok_or_else(|| invalid("log read count invalid"))?,
                )?,
                Err(error) if error == rustix::io::Errno::AGAIN => return Ok(()),
                Err(error) if error == rustix::io::Errno::INTR => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use serde_json::json;

    use super::*;

    #[test]
    fn the_cap_keeps_a_prefix_then_one_truncation_line_across_stages() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("build.log");
        let mut log = CappedLog::open(&path, 10)?;
        log.append(b"header\n")?;
        assert!(!log.truncated());
        log.append(b"abcdef")?;
        assert!(log.truncated());
        log.append(b"dropped")?;
        drop(log);
        // A later stage reopens the log and adds nothing, not even a second line.
        let mut later = CappedLog::open(&path, 10)?;
        assert!(later.truncated());
        later.append(b"late\n")?;
        let mut expected = b"header\nabc".to_vec();
        expected.extend_from_slice(TRUNCATED);
        assert_eq!(std::fs::read(&path)?, expected);
        assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn output_that_exactly_fills_the_cap_is_not_truncated() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("build.log");
        let mut log = CappedLog::open(&path, 4)?;
        log.append(b"ab")?;
        log.append(b"cd")?;
        log.append(b"")?;
        assert!(!log.truncated());
        assert_eq!(std::fs::read(&path)?, b"abcd");
        Ok(())
    }

    #[test]
    fn a_log_path_that_is_a_symbolic_link_is_refused() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let elsewhere = directory.path().join("elsewhere");
        std::fs::write(&elsewhere, b"")?;
        let link = directory.path().join("build.log");
        std::os::unix::fs::symlink(&elsewhere, &link)?;
        assert!(CappedLog::open(&link, 10).is_err());
        assert_eq!(std::fs::read(&elsewhere)?, b"");
        Ok(())
    }

    #[test]
    fn the_helper_takes_both_settings_out_of_the_stage_environment() -> io::Result<()> {
        let mut message = json!({"env": {LOG_ENV: "/logs/a.log", LOG_LIMIT_ENV: "16", "KEEP": "1"}});
        let target = take_log(&mut message)?.ok_or_else(|| io::Error::other("no log target"))?;
        assert_eq!(target.path, Path::new("/logs/a.log"));
        assert_eq!(target.limit, 16);
        assert_eq!(message.get("env"), Some(&json!({"KEEP": "1"})));
        assert!(take_log(&mut json!({"env": {}}))?.is_none());
        assert!(take_log(&mut json!({"command": ["lake"]}))?.is_none());
        for env in [
            json!({LOG_ENV: "/logs/a.log"}),
            json!({LOG_LIMIT_ENV: "16"}),
            json!({LOG_ENV: "a.log", LOG_LIMIT_ENV: "16"}),
            json!({LOG_ENV: "/logs/a.log", LOG_LIMIT_ENV: "lots"}),
            json!({LOG_ENV: null, LOG_LIMIT_ENV: "16"}),
        ] {
            assert!(take_log(&mut json!({ "env": env })).is_err(), "{env}");
        }
        Ok(())
    }
}
