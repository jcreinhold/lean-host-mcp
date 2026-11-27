//! A build's log: one file under the store that every step of the build
//! appends to, capped at `[build] log_limit_mib`.
//!
//! Requirement: a build's log never grows past its cap, however much Lake
//! writes. Past the cap the log keeps a prefix and one line that says the rest
//! was dropped, and the server keeps reading Lake's output so Lake never
//! blocks on a full pipe.

use std::fs::File;
use std::io::{self, PipeReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl};

/// The line that ends a log that reached its cap.
pub(crate) const TRUNCATED: &[u8] = b"\n[lean-host-mcp: log limit reached; later output dropped]\n";

/// How many bytes one read takes from Lake's pipe.
const READ_BYTES: usize = 16 * 1024;

/// An append-only log file that keeps at most `limit` bytes of output.
#[derive(Debug)]
pub(crate) struct CappedLog {
    file: File,
    len: u64,
    limit: u64,
}

impl CappedLog {
    /// Open or create the log at `path` with mode 0600 for appending. Bytes
    /// already in the file count toward `limit`.
    ///
    /// # Errors
    /// Refuses a path that is a symbolic link or not a regular file, and
    /// returns other I/O errors.
    pub(crate) fn open(path: &Path, limit: u64) -> io::Result<Self> {
        let file = File::from(rustix::fs::open(
            path,
            OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the build log is not a regular file",
            ));
        }
        Ok(Self {
            file,
            len: metadata.len(),
            limit,
        })
    }

    /// Whether the log has reached its cap and dropped output.
    pub(crate) const fn truncated(&self) -> bool {
        self.len > self.limit
    }

    /// Append what fits of `bytes`; at the cap, append the truncation line
    /// once and drop the rest, now and later.
    ///
    /// # Errors
    /// Returns the write error.
    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() || self.truncated() {
            return Ok(());
        }
        let room = usize::try_from(self.limit.saturating_sub(self.len)).unwrap_or(usize::MAX);
        let (kept, dropped) = bytes.split_at(bytes.len().min(room));
        self.file.write_all(kept)?;
        self.len = self.len.saturating_add(kept.len() as u64);
        if !dropped.is_empty() {
            self.file.write_all(TRUNCATED)?;
            self.len = self.len.saturating_add(TRUNCATED.len() as u64);
        }
        Ok(())
    }
}

/// The read end of the pipe a step's stdout and stderr share, copied into
/// the log without blocking.
#[derive(Debug)]
pub(crate) struct LogSink {
    reader: PipeReader,
    closed: bool,
    write_failed: bool,
}

impl LogSink {
    /// Read `reader` without blocking.
    ///
    /// # Errors
    /// Returns the error from setting the pipe nonblocking.
    pub(crate) fn new(reader: PipeReader) -> io::Result<Self> {
        let flags = fcntl_getfl(&reader)?;
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK)?;
        Ok(Self {
            reader,
            closed: false,
            write_failed: false,
        })
    }

    /// Copy what the pipe holds now into `log`, up to a bound per call so a
    /// step that writes without pause cannot hold the loop. Returns whether
    /// more may wait. A failed write drops output but keeps the pipe drained.
    pub(crate) fn copy_into(&mut self, log: &mut CappedLog) -> bool {
        if self.closed {
            return false;
        }
        let mut buffer = [0_u8; READ_BYTES];
        for _ in 0..32 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.closed = true;
                    return false;
                }
                Ok(count) => {
                    let written = buffer.get(..count).map_or_else(
                        || Err(io::Error::other("read count past the buffer")),
                        |chunk| log.append(chunk),
                    );
                    if let Err(error) = written
                        && !self.write_failed
                    {
                        self.write_failed = true;
                        tracing::warn!(%error, "cannot write to the build log; dropping Lake's output");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return false,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot read Lake's output");
                    self.closed = true;
                    return false;
                }
            }
        }
        true
    }

    /// Copy everything the pipe holds now into `log`, without waiting for
    /// more.
    pub(crate) fn drain_into(&mut self, log: &mut CappedLog) {
        while self.copy_into(log) {}
    }

    /// The pipe to wake on, while it is open.
    pub(crate) fn awaited(&self) -> Option<BorrowedFd<'_>> {
        (!self.closed).then(|| self.reader.as_fd())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn the_cap_keeps_a_prefix_then_one_truncation_line_across_steps() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("build.log");
        let mut log = CappedLog::open(&path, 10)?;
        log.append(b"header\n")?;
        log.append(b"abcdef")?;
        assert!(log.truncated());
        log.append(b"dropped")?;
        drop(log);
        let mut reopened = CappedLog::open(&path, 10)?;
        assert!(reopened.truncated());
        reopened.append(b"more")?;
        let mut expected = b"header\nabc".to_vec();
        expected.extend_from_slice(TRUNCATED);
        assert_eq!(fs::read(&path)?, expected);
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn output_that_exactly_fills_the_cap_is_not_truncated() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("build.log");
        let mut log = CappedLog::open(&path, 4)?;
        log.append(b"ab")?;
        log.append(b"cd")?;
        log.append(b"")?;
        assert!(!log.truncated());
        assert_eq!(fs::read(&path)?, b"abcd");
        Ok(())
    }

    #[test]
    fn a_log_path_that_is_a_symbolic_link_is_refused() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let target = tmp.path().join("target");
        fs::write(&target, b"")?;
        let link = tmp.path().join("build.log");
        std::os::unix::fs::symlink(&target, &link)?;
        assert!(CappedLog::open(&link, 10).is_err());
        Ok(())
    }

    #[test]
    fn a_sink_copies_a_pipe_into_the_log_up_to_the_cap() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("build.log");
        let mut log = CappedLog::open(&path, 5)?;
        let (reader, mut writer) = io::pipe()?;
        let mut sink = LogSink::new(reader)?;
        writer.write_all(b"hello world")?;
        drop(writer);
        // A process another test forks keeps a copy of the write end until it
        // execs, so wait for the pipe to close instead of expecting it closed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        sink.drain_into(&mut log);
        while sink.awaited().is_some() && std::time::Instant::now() < deadline {
            crate::lean_run::wait_readable(&[sink.awaited()], std::time::Duration::from_millis(50));
            sink.drain_into(&mut log);
        }
        assert!(sink.awaited().is_none(), "the sink saw the pipe close");
        let mut expected = b"hello".to_vec();
        expected.extend_from_slice(TRUNCATED);
        assert_eq!(fs::read(&path)?, expected);
        Ok(())
    }
}
