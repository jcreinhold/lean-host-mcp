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
    Ok(())
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
        let mut bytes = Vec::new();
        let read = input.take(OUTPUT_LIMIT as u64 + 1).read_to_end(&mut bytes);
        if read.is_err() || bytes.len() > OUTPUT_LIMIT {
            let _killed = child.kill();
        }
        let status = child.wait()?;
        read?;
        if bytes.len() > OUTPUT_LIMIT {
            return Err(io::Error::other("whole-library lint output exceeds retained bound"));
        }
        let code = status
            .code()
            .ok_or_else(|| io::Error::other("whole-library lint interrupted"))?;
        let output = String::from_utf8(bytes).map_err(io::Error::other)?;
        checks.push(Check {
            name: name.to_owned(),
            exit_code: code,
            output,
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
