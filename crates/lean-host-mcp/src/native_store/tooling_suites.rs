//! Build and run the four fixed native tooling suites inside one committed writer.
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::Read as _,
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use super::pinned_lake_config;

const CONFIG: &[u8] = include_bytes!("tooling_suites.toml");
const BUILD_DIR: &str = ".lake/build/tooling-tests";
const MAX_ENV_BYTES: u64 = 64 * 1024;

pub(crate) const OPERATION: &str = "lean_tooling_suites_v1";
const STAGE: &str = "lean-tooling-suites";
const SUITES: [&str; 4] = [
    "shake-safe-tests",
    "style-lint-tests",
    "probe-uses-tests",
    "regroup-variables-tests",
];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Check {
    name: String,
    pub(super) exit_code: i32,
    pub(super) output: String,
    pub(super) output_truncated: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u8,
    stage: String,
    checks: Vec<Check>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preparation_failure: Option<Check>,
}

impl Receipt {
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let result: Self = serde_json::from_slice(bytes)?;
        if result.version != 1 || result.stage != STAGE {
            return Err(io::Error::other("invalid tooling suite result"));
        }
        if let Some(failure) = &result.preparation_failure {
            if result.checks.is_empty()
                && matches!(failure.name.as_str(), "tooling-build" | "target-import-environment")
                && failure.exit_code > 0
            {
                return Ok(result);
            }
            return Err(io::Error::other("invalid tooling preparation failure"));
        }
        let Some((last, earlier)) = result.checks.split_last() else {
            return Err(io::Error::other("empty tooling suite result"));
        };
        if result.checks.len() > SUITES.len()
            || result.checks.iter().zip(SUITES).any(|(row, name)| row.name != name)
            || earlier.iter().any(|row| row.exit_code != 0)
            || (result.checks.len() < SUITES.len() && last.exit_code == 0)
        {
            return Err(io::Error::other("invalid tooling suite result"));
        }
        Ok(result)
    }
}

pub(crate) fn package_root(root: &Path) -> io::Result<PathBuf> {
    let package = root.join("scripts/lean");
    if package.canonicalize()? != package || !package.is_dir() {
        return Err(io::Error::other("tooling package path is not canonical"));
    }
    Ok(package)
}

pub(crate) fn check(root: &Path, config: &[u8; 32], manifest: &[u8; 32]) -> io::Result<()> {
    let package = package_root(root)?;
    pinned_lake_config::absent(&package.join("lakefile.lean"))?;
    if pinned_lake_config::file_bytes(&package.join("lakefile.toml"))? != CONFIG {
        return Err(io::Error::other(
            "tooling package configuration differs from fixed contract",
        ));
    }
    pinned_lake_config::check_dependency_free(&package)?;
    pinned_lake_config::validate_root_config(root, config)?;
    pinned_lake_config::check(root, manifest)?;
    for dir in [
        package.join(".lake"),
        root.join(".lake"),
        root.join(".lake/build"),
        root.join(BUILD_DIR),
        root.join(BUILD_DIR).join("bin"),
    ] {
        match dir.symlink_metadata() {
            Ok(meta) if meta.is_dir() && dir.canonicalize()? == dir => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Err(io::Error::other("tooling output directory is not canonical")),
        }
    }
    Ok(())
}

pub(super) fn capture(mut command: Command, name: &str) -> io::Result<Check> {
    let (input, output) = UnixStream::pair()?;
    let output: std::os::fd::OwnedFd = output.into();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .spawn()?;
    drop(command);
    let preview = super::whole_lint::diagnostic_preview(input);
    let status = child.wait()?;
    let (output, output_truncated) = preview?;
    Ok(Check {
        name: name.into(),
        exit_code: status
            .code()
            .ok_or_else(|| io::Error::other("tooling stage interrupted"))?,
        output,
        output_truncated,
    })
}

fn lean_path(mut command: Command) -> io::Result<Result<std::ffi::OsString, Check>> {
    use std::os::unix::ffi::OsStringExt as _;

    let mut child = command
        .args(["--reconfigure", "--no-build", "env", "/usr/bin/printenv", "LEAN_PATH"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Vec::new();
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing Lake environment output"))?;
    let read = output.take(MAX_ENV_BYTES + 1).read_to_end(&mut bytes);
    let code = child
        .wait()?
        .code()
        .ok_or_else(|| io::Error::other("Lake environment interrupted"))?;
    read?;
    if code != 0 {
        return Ok(Err(Check {
            name: "target-import-environment".into(),
            exit_code: code,
            output: String::new(),
            output_truncated: false,
        }));
    }
    let bytes = bytes
        .strip_suffix(b"\n")
        .ok_or_else(|| io::Error::other("invalid Lake import path"))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ENV_BYTES || bytes.contains(&b'\n') || bytes.contains(&0) {
        return Err(io::Error::other("invalid Lake import path"));
    }
    let paths = std::ffi::OsString::from_vec(bytes.to_vec());
    if !std::env::split_paths(&paths).all(|path| path.is_absolute()) {
        return Err(io::Error::other("Lake import path is not absolute"));
    }
    Ok(Ok(paths))
}

pub(super) fn run(
    root: &Path,
    config: &[u8; 32],
    manifest: &[u8; 32],
    authorize: impl Fn() -> io::Result<Duration>,
) -> io::Result<Receipt> {
    authorize()?;
    check(root, config, manifest)?;
    let package = package_root(root)?;
    let lake = super::lake_executable(root)?;
    let sysroot = lake
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("missing Lean sysroot"))?;
    let lake_command = |cwd: &Path| {
        let mut command = Command::new(&lake);
        command
            .current_dir(cwd)
            .arg("--no-cache")
            .env("LEAN_SYSROOT", sysroot)
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1");
        command
    };
    let mut receipt = Receipt {
        version: 1,
        stage: STAGE.into(),
        checks: Vec::new(),
        preparation_failure: None,
    };
    authorize()?;
    let paths = match lean_path(lake_command(root))? {
        Ok(paths) => paths,
        Err(failure) => {
            receipt.preparation_failure = Some(failure);
            return Ok(receipt);
        }
    };
    authorize()?;
    let mut build = lake_command(&package);
    build.arg("build").args(SUITES.map(|name| format!("@/{name}")));
    let outcome = capture(build, "tooling-build")?;
    if outcome.exit_code != 0 {
        receipt.preparation_failure = Some(outcome);
        return Ok(receipt);
    }

    // Validate every fixed output before allowing the first suite to run.
    let mut executables = Vec::new();
    for name in SUITES {
        let path = root.join(BUILD_DIR).join("bin").join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 || path.canonicalize()? != path {
            return Err(io::Error::other(
                "tooling build did not produce the expected executable",
            ));
        }
        executables.push(path);
    }
    for (name, path) in SUITES.into_iter().zip(executables) {
        authorize()?;
        let mut command = Command::new(path);
        command
            .current_dir(root)
            .env("LEAN_SYSROOT", sysroot)
            .env("LEAN_PATH", &paths);
        let outcome = capture(command, name)?;
        let passed = outcome.exit_code == 0;
        receipt.checks.push(outcome);
        if !passed {
            break;
        }
    }
    authorize()?;
    Ok(receipt)
}
