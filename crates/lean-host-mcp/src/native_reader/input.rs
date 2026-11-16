//! Explicit bounded stdin, stored in the existing native generation's inputs.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{FileType, Mode, OFlags, fcntl_getfl, fcntl_setfl, fstat, open, openat};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    pub(super) sha256: String,
    pub(super) bytes: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn ready(deadline: Instant, still_alive: &(impl Fn() -> io::Result<bool> + ?Sized)) -> io::Result<()> {
    if !still_alive()? {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "native stdin caller ended or cancelled",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "native stdin delivery deadline elapsed",
        ));
    }
    Ok(())
}

struct Restore<'a, F: AsFd> {
    input: &'a F,
    flags: OFlags,
}
impl<F: AsFd> Drop for Restore<'_, F> {
    fn drop(&mut self) {
        let _restored = fcntl_setfl(self.input, self.flags);
    }
}

/// Consume exactly one bounded byte stream before spawning a native bootstrap.
/// No payload is admitted when EOF, caller identity or the deadline is missing.
fn capture_bytes<F: AsFd>(
    input: &F,
    deadline: Instant,
    still_alive: &(impl Fn() -> io::Result<bool> + ?Sized),
) -> io::Result<Vec<u8>> {
    ready(deadline, still_alive)?;
    // Changing O_NONBLOCK on a terminal/socket could affect a caller's shared
    // description after an abrupt controller exit. The supported producer owns
    // an unshared pipe read end or a regular input file until capture completes.
    let metadata = fstat(input)?;
    if !matches!(
        FileType::from_raw_mode(metadata.st_mode),
        FileType::RegularFile | FileType::Fifo
    ) {
        return Err(invalid("native stdin requires an owned pipe or regular file"));
    }
    let flags = fcntl_getfl(input)?;
    fcntl_setfl(input, flags | OFlags::NONBLOCK)?;
    let _restore = Restore { input, flags };
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        ready(deadline, still_alive)?;
        match rustix::io::read(input, &mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(count) => {
                if count > MAX_BYTES.saturating_sub(bytes.len()) {
                    return Err(invalid("native stdin exceeds 16 MiB"));
                }
                bytes.extend_from_slice(
                    buffer
                        .get(..count)
                        .ok_or_else(|| invalid("native stdin read count invalid"))?,
                );
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => {
                std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn receipt(bytes: &[u8]) -> io::Result<Receipt> {
    let mut sha256 = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut sha256, "{byte:02x}").map_err(io::Error::other)?;
    }
    Ok(Receipt {
        sha256,
        bytes: bytes.len().try_into().map_err(io::Error::other)?,
    })
}

fn directory(path: &Path) -> io::Result<File> {
    Ok(File::from(open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?))
}

/// Publish the literal input file only within an already reserved generation.
fn store(path: &Path, bytes: &[u8]) -> io::Result<Receipt> {
    if bytes.len() > MAX_BYTES {
        return Err(invalid("native stdin exceeds 16 MiB"));
    }
    let directory = directory(path)?;
    let mut file = File::from(openat(
        &directory,
        "stdin",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    file.write_all(bytes)?;
    file.sync_all()?;
    receipt(bytes)
}

fn identity(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

/// Hash the exact read-only descriptor that will become the payload's fd 0.
/// Existing input aliases, mutation and oversized input refuse before spawn.
fn for_payload(path: &Path) -> io::Result<(File, Receipt)> {
    let directory = directory(path)?;
    let mut file = File::from(openat(
        &directory,
        "stdin",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    let before = file.metadata()?;
    if !before.is_file()
        || before.nlink() != 1
        || before.len() > MAX_BYTES as u64
        || before.uid() != rustix::process::getuid().as_raw()
    {
        return Err(invalid("native stdin is not one bounded owned regular file"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES || identity(&before) != identity(&file.metadata()?) || bytes.len() as u64 != before.len()
    {
        return Err(invalid("native stdin changed before payload admission"));
    }
    let receipt = receipt(&bytes)?;
    file.rewind()?;
    Ok((file, receipt))
}

/// Exact bytes staged under this generation, offered only over private control.
/// The controlled bootstrap must additionally bind this path to its authenticated
/// generation's literal `inputs/stdin` before opening it.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StagedInput {
    pub(super) path: PathBuf,
    pub(super) sha256: String,
    pub(super) bytes: u64,
}

pub(super) fn capture<F: AsFd>(
    input: &F,
    directory: &Path,
    deadline: Instant,
    still_alive: &(impl Fn() -> io::Result<bool> + ?Sized),
) -> io::Result<StagedInput> {
    let bytes = capture_bytes(input, deadline, still_alive)?;
    ready(deadline, still_alive)?;
    let stored = store(directory, &bytes)?;
    ready(deadline, still_alive)?;
    Ok(StagedInput {
        path: directory.join("stdin"),
        sha256: stored.sha256,
        bytes: stored.bytes,
    })
}

/// Verify and rewind the same read-only descriptor subsequently installed at fd0.
pub(super) fn open_verified(staged: &StagedInput) -> io::Result<File> {
    if !staged.path.is_absolute()
        || staged.path.file_name().is_none_or(|name| name != "stdin")
        || staged.path.canonicalize()? != staged.path
        || staged.bytes > MAX_BYTES as u64
        || staged.sha256.len() != 64
        || !staged
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("native stdin descriptor is invalid"));
    }
    let parent = staged
        .path
        .parent()
        .ok_or_else(|| invalid("native stdin has no owner directory"))?;
    let (file, actual) = for_payload(parent)?;
    if actual.bytes != staged.bytes || actual.sha256 != staged.sha256 {
        return Err(invalid("native stdin differs from its offered byte receipt"));
    }
    Ok(file)
}

#[cfg(test)]
mod tests;
