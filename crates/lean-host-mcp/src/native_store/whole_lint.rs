//! The fixed Proofs whole-library lint sequence, inside one committed writer.
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    fs, io,
    io::{Read as _, Write as _},
    path::Path,
    process::{Command, Stdio},
};

pub(crate) const OPERATION: &str = "lake_whole_library_lint_v1";

const STEPS: [(&str, &[&str]); 4] = [
    (
        "readiness",
        &["build", "Proofs", "runLinter", "style-lint", "@mathlib/lint-style"],
    ),
    ("environment-lint", &["lint", "--", "--no-build"]),
    ("mathlib-style-lint", &["exe", "@mathlib/lint-style"]),
    ("source-style-lint", &["exe", "style-lint"]),
];
const OUTPUT_LIMIT: usize = 2048;
const CONFIG_LIMIT: u64 = 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Check {
    name: String,
    exit_code: i32,
    output: String,
    #[serde(default)]
    output_truncated: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u8,
    stage: String,
    checks: Vec<Check>,
}

impl Receipt {
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let result: Self = serde_json::from_slice(bytes)?;
        if result.version != 1
            || result.stage != "whole-library-lint"
            || result.checks.is_empty()
            || result.checks.len() > STEPS.len()
            || result
                .checks
                .iter()
                .zip(STEPS)
                .any(|(check, step)| check.name != step.0)
            || result.checks.iter().rev().skip(1).any(|check| check.exit_code != 0)
            || (result.checks.len() < STEPS.len() && result.checks.last().is_some_and(|c| c.exit_code == 0))
        {
            return Err(io::Error::other("invalid whole-library lint result"));
        }
        Ok(result)
    }
}

pub(crate) fn check_config(root: &Path, expected: &[u8; 32]) -> io::Result<()> {
    read_checked_config(root, expected).map(drop)
}

pub(crate) fn read_checked_config(root: &Path, expected: &[u8; 32]) -> io::Result<Vec<u8>> {
    let path = root.join("lakefile.lean");
    let file = fs::File::from(rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    if !file.metadata()?.is_file()
        || path.canonicalize()? != path
        || root.join("lakefile.toml").symlink_metadata().is_ok()
    {
        return Err(io::Error::other(
            "whole-library lint requires the admitted Lean Lake configuration",
        ));
    }
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONFIG_LIMIT || Sha256::digest(&bytes).as_slice() != expected {
        return Err(io::Error::other("whole-library lint configuration changed"));
    }
    Ok(bytes)
}

/// Keep the beginning of the diagnostic stream and drain the rest so its size
/// never turns a real lint exit into an artificial pipe failure.
fn diagnostic_preview(mut input: impl io::Read) -> io::Result<(String, bool)> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((OUTPUT_LIMIT as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    let clipped = bytes.len() > OUTPUT_LIMIT;
    bytes.truncate(OUTPUT_LIMIT);
    if clipped {
        io::copy(&mut input, &mut io::sink())?;
    }
    match String::from_utf8(bytes) {
        Ok(output) => Ok((output, clipped)),
        Err(error) => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            Ok((String::from_utf8(bytes).map_err(io::Error::other)?, true))
        }
    }
}

pub(crate) fn run(
    root: &Path,
    lake: &Path,
    expected: &[u8; 32],
    authorize: impl Fn() -> io::Result<std::time::Duration>,
) -> io::Result<()> {
    let mut checks = Vec::new();
    let mut paths = Vec::new();
    for name in [
        ".lake/build/bin/runLinter",
        ".lake/build/bin/style-lint",
        ".lake/packages/mathlib/.lake/build/bin/lint-style",
    ] {
        let path = root.join(name);
        let metadata = path.symlink_metadata()?;
        use std::os::unix::fs::MetadataExt as _;
        if path.canonicalize()? != path || !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(io::Error::other(
                "whole-library lint requires canonical prebuilt executables",
            ));
        }
        paths.push((path, metadata));
    }
    for (name, args) in STEPS {
        check_config(root, expected)?;
        for (path, initial) in &paths {
            use std::os::unix::fs::MetadataExt as _;
            let current = path.symlink_metadata()?;
            let identity = |m: &fs::Metadata| (m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec());
            if path.canonicalize()? != *path || identity(&current) != identity(initial) {
                return Err(io::Error::other("whole-library lint executable changed"));
            }
        }
        authorize()?;
        let (input, output) = std::os::unix::net::UnixStream::pair()?;
        let output: std::os::fd::OwnedFd = output.into();
        let mut child = Command::new(lake)
            .current_dir(root)
            .args(["--no-build", "--no-cache"])
            .args(args)
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()?;
        let preview = diagnostic_preview(input);
        let status = child.wait()?;
        let (output, output_truncated) = preview?;
        let code = status
            .code()
            .ok_or_else(|| io::Error::other("whole-library lint interrupted"))?;
        checks.push(Check {
            name: name.to_owned(),
            exit_code: code,
            output,
            output_truncated,
        });
        if code != 0 {
            break;
        }
    }
    check_config(root, expected)?;
    authorize()?;
    let bytes = serde_json::to_vec(&Receipt {
        version: 1,
        stage: "whole-library-lint".into(),
        checks,
    })?;
    io::stdout().lock().write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_preview_drains_oversized_output_without_exposing_partial_utf8() -> io::Result<()> {
        let mut data = vec![b'x'; OUTPUT_LIMIT - 1];
        data.extend_from_slice("é and the rest".as_bytes());
        let mut input = io::Cursor::new(&data);
        let (preview, truncated) = diagnostic_preview(&mut input)?;
        assert!(truncated);
        assert_eq!(preview.len(), OUTPUT_LIMIT - 1);
        assert_eq!(input.position(), data.len() as u64);
        assert_eq!(
            diagnostic_preview(&b"ordinary finding\n"[..])?,
            ("ordinary finding\n".to_owned(), false)
        );
        Ok(())
    }
}
