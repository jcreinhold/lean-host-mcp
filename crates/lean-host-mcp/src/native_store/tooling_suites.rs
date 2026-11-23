//! Run the four fixed prebuilt tooling suites inside one committed writer.
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    fs, io,
    io::Read as _,
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

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
struct Check {
    name: String,
    exit_code: i32,
    output: String,
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
        let Some((last, earlier)) = result.checks.split_last() else {
            return Err(io::Error::other("empty tooling suite result"));
        };
        if result.version != 1
            || result.stage != STAGE
            || result.checks.len() > SUITES.len()
            || result.checks.iter().zip(SUITES).any(|(row, name)| row.name != name)
            || earlier.iter().any(|row| row.exit_code != 0)
            || (result.checks.len() < SUITES.len() && last.exit_code == 0)
        {
            return Err(io::Error::other("invalid tooling suite result"));
        }
        Ok(result)
    }
}

fn executable(root: &Path, name: &str, expected: &[u8; 32]) -> io::Result<PathBuf> {
    let path = root.join(".lake/build/bin").join(name);
    let mut file = fs::File::from(rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 || path.canonicalize()? != path {
        return Err(io::Error::other(
            "tooling suite requires a canonical prebuilt executable",
        ));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    if hash.finalize().as_slice() != expected {
        return Err(io::Error::other("prebuilt tooling suite changed"));
    }
    Ok(path)
}

pub(crate) fn check(
    root: &Path,
    hashes: &[[u8; 32]; 4],
    authorize: &impl Fn() -> io::Result<Duration>,
) -> io::Result<()> {
    for (name, hash) in SUITES.into_iter().zip(hashes) {
        authorize()?;
        executable(root, name, hash)?;
    }
    Ok(())
}

pub(super) fn run(
    root: &Path,
    hashes: &[[u8; 32]; 4],
    authorize: impl Fn() -> io::Result<Duration>,
) -> io::Result<Receipt> {
    let toolchain = crate::ToolchainId::from_lake_root(root).map_err(io::Error::other)?;
    let sysroot = toolchain.elan_dir().map_err(io::Error::other)?.canonicalize()?;
    let artifact_roots = lean_toolchain::shared_writer::project_artifact_roots(root)?;
    let lean_path = std::env::join_paths(
        artifact_roots
            .iter()
            .map(|root| root.join(crate::lake_meta::BUILD_LIB_REL)),
    )
    .map_err(io::Error::other)?;
    check(root, hashes, &authorize)?;
    let mut checks = Vec::new();
    for (name, hash) in SUITES.into_iter().zip(hashes) {
        authorize()?;
        let path = executable(root, name, hash)?;
        let (input, output) = UnixStream::pair()?;
        let output: std::os::fd::OwnedFd = output.into();
        authorize()?;
        let mut child = Command::new(path)
            .current_dir(root)
            .env("LEAN_SYSROOT", &sysroot)
            .env("LEAN_PATH", &lean_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output))
            .spawn()?;
        let preview = super::whole_lint::diagnostic_preview(input);
        let status = child.wait()?;
        let (output, output_truncated) = preview?;
        let exit_code = status
            .code()
            .ok_or_else(|| io::Error::other("tooling suite interrupted"))?;
        checks.push(Check {
            name: name.into(),
            exit_code,
            output,
            output_truncated,
        });
        if exit_code != 0 {
            break;
        }
    }
    authorize()?;
    Ok(Receipt {
        version: 1,
        stage: STAGE.into(),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "uses installed Lean and leanc to compile two tiny disposable programs"]
    fn prebuilt_lean_suites_keep_order_and_refuse_missing_inputs() -> io::Result<()> {
        let project = tempfile::tempdir()?;
        let root = project.path().canonicalize()?;
        let bin = root.join(".lake/build/bin");
        fs::create_dir_all(&bin)?;
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.36.0\n")?;
        fs::write(root.join("lakefile.toml"), "name = \"native_suite_fixture\"\n")?;
        fs::write(
            root.join("lake-manifest.json"),
            r#"{"version":"1.1.0","packagesDir":".lake/packages","packages":[]}"#,
        )?;
        let toolchain = crate::ToolchainId::from_lake_root(&root).map_err(io::Error::other)?;
        let sysroot = toolchain.elan_dir().map_err(io::Error::other)?;
        let compile = |exit: u32, output: &Path| -> io::Result<[u8; 32]> {
            fs::write(
                root.join("Main.lean"),
                format!("def main : IO UInt32 := do\n  IO.println \"actual Lean suite\"\n  return {exit}\n"),
            )?;
            assert!(
                Command::new(sysroot.join("bin/lean"))
                    .current_dir(&root)
                    .args(["--c=main.c", "Main.lean"])
                    .status()?
                    .success()
            );
            assert!(
                Command::new(sysroot.join("bin/leanc"))
                    .current_dir(&root)
                    .args(["-o"])
                    .arg(output)
                    .arg("main.c")
                    .status()?
                    .success()
            );
            Ok(Sha256::digest(fs::read(output)?).into())
        };
        let first = bin.join(SUITES[0]);
        let good = compile(0, &first)?;
        for name in &SUITES[1..] {
            fs::copy(&first, bin.join(name))?;
        }
        let allow = || Ok(Duration::from_secs(10));
        let mut hashes = [good; 4];
        let receipt = run(&root, &hashes, allow)?;
        let receipt = Receipt::parse(&serde_json::to_vec(&receipt)?)?;
        assert_eq!(receipt.checks.len(), 4);
        assert!(
            receipt
                .checks
                .iter()
                .all(|row| row.exit_code == 0 && row.output == "actual Lean suite\n")
        );
        hashes[1] = compile(7, &bin.join(SUITES[1]))?;
        let receipt = run(&root, &hashes, allow)?;
        let receipt = Receipt::parse(&serde_json::to_vec(&receipt)?)?;
        assert_eq!(receipt.checks.len(), 2);
        assert_eq!(receipt.checks[1].exit_code, 7);
        fs::remove_file(bin.join(SUITES[3]))?;
        match run(&root, &hashes, allow) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
            Ok(_) => panic!("a missing final suite must refuse the sequence"),
        }
        Ok(())
    }
}
