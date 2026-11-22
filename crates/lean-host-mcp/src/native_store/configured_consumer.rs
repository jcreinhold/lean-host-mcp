//! Fixed Lake check of one captured project source under exclusive writer custody.
use std::{
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const MAX_SOURCE: u64 = 16 * 1024 * 1024;
const MAX_RECEIPT: usize = 60 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceIdentity {
    pub(super) file: PathBuf,
    pub(super) sha256: [u8; 32],
}

impl SourceIdentity {
    pub(super) fn valid(&self) -> bool {
        self.file.as_os_str().len() <= 1024
            && self.file.extension().is_some_and(|ext| ext == "lean")
            && self.file.components().all(|part| matches!(part, Component::Normal(_)))
            && self.file.to_str().is_some_and(|path| {
                !path.starts_with('-') && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
            })
    }

    pub(super) fn digest(&self, root: &Path) -> io::Result<[u8; 32]> {
        let path = root.join(&self.file);
        if path.canonicalize()?.as_os_str() != path.as_os_str() {
            return Err(io::Error::other("configured consumer source alias"));
        }
        use rustix::fs::{Mode, OFlags, open};
        let file = std::fs::File::from(open(
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?);
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("configured consumer requires a regular file"));
        }
        let mut hash = Sha256::new();
        let mut input = file.take(MAX_SOURCE + 1);
        let mut buffer = [0; 8192];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        if input.limit() == 0 {
            return Err(io::Error::other("source exceeds the admitted file limit"));
        }
        Ok(hash.finalize().into())
    }

    pub(super) fn check(&self, root: &Path) -> io::Result<()> {
        if self.digest(root)? != self.sha256 {
            return Err(io::Error::other("configured consumer source changed"));
        }
        Ok(())
    }

    pub(super) fn command(&self, root: &Path, assets: &Path) -> io::Result<Command> {
        let mut command = Command::new(crate::writer_preparation::lake_executable(root)?);
        command
            .current_dir(root)
            .args(["--log-level=error", "--no-build", "--no-cache", "lean"])
            .arg(&self.file)
            .args(["--", "--json"])
            .env("LEAN_NUM_THREADS", "4")
            .env("LAKE_CACHE_DIR", assets)
            .env("TMPDIR", assets);
        Ok(command)
    }

    pub(super) fn receipt(&self, root: &Path, output: Output) -> serde_json::Value {
        let unavailable = |reason| serde_json::json!({"version":1,"stage":"configured-consumer","status":"unavailable","reason":reason});
        if self.check(root).is_err() {
            return unavailable("source_changed");
        }
        let (Some(exit_code), Ok(stdout), Ok(stderr)) = (
            output.status.code(),
            String::from_utf8(output.stdout),
            String::from_utf8(output.stderr),
        ) else {
            return unavailable("incomplete_output");
        };
        if stdout.len().saturating_add(stderr.len()) > MAX_RECEIPT {
            return unavailable("output_exceeds_limit");
        }
        let body = serde_json::json!({
            "version":1, "stage":"configured-consumer", "file":self.file,
            "source_sha256":self.sha256, "exit_code":exit_code, "stdout":stdout, "stderr":stderr
        });
        if body.to_string().len() > MAX_RECEIPT {
            return unavailable("output_exceeds_limit");
        }
        body
    }
}
