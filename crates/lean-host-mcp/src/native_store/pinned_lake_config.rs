//! Validate the manifest-driven configuration selection before Lake can load it.
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs, io,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_INPUT: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    packages_dir: String,
    packages: Vec<Package>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    rev: String,
    config_file: String,
    sub_dir: Option<PathBuf>,
}

fn invalid(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.into())
}

pub(super) fn absent(path: &Path) -> io::Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(invalid(format!(
            "unsupported Lake configuration input: {}",
            path.display()
        ))),
    }
}

pub(super) fn file_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let file = fs::File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    if !file.metadata()?.is_file() || path.canonicalize()? != path {
        return Err(invalid("Lake configuration input is not canonical"));
    }
    read_bounded(file)
}

fn read_bounded(input: impl io::Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(invalid("Lake configuration input exceeds limit"));
    }
    Ok(bytes)
}

fn git(repo: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("--git-dir")
        .arg(repo.join(".git"))
        .arg("--work-tree")
        .arg(repo)
        .args(args)
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("GIT_")) {
            command.env_remove(key);
        }
    }
    let mut child = command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let bytes = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git stdout was not piped"))
        .and_then(read_bounded);
    if !child.wait()?.success() {
        return Err(invalid("required Git object is unavailable locally"));
    }
    bytes
}

pub(super) fn check(root: &Path, expected_manifest: &[u8; 32]) -> io::Result<()> {
    checked_package_order(root, expected_manifest).map(|_| ())
}

pub(super) fn checked_package_order(root: &Path, expected_manifest: &[u8; 32]) -> io::Result<Vec<PathBuf>> {
    absent(&root.join(".lake/package-overrides.json"))?;
    let bytes = file_bytes(&root.join("lake-manifest.json"))?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != *expected_manifest {
        return Err(invalid("admitted Lake manifest changed"));
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.packages_dir != ".lake/packages" || manifest.packages.len() > 64 {
        return Err(invalid("unsupported Lake package layout"));
    }
    let mut seen = BTreeSet::new();
    let mut ordered = vec![root.to_owned()];
    for package in manifest.packages {
        // Lake decodes the Name and materializes it with escape := false.
        let name = package
            .name
            .strip_prefix('«')
            .and_then(|name| name.strip_suffix('»'))
            .unwrap_or(&package.name);
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !seen.insert(name.to_owned())
            || package.kind != "git"
            || package.sub_dir.is_some()
            || package.rev.len() != 40
            || !package.rev.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid("unsupported locked Lake package"));
        }
        let alternate = match package.config_file.as_str() {
            "lakefile.lean" => "lakefile.toml",
            "lakefile.toml" => "lakefile.lean",
            _ => {
                return Err(invalid(
                    "Lake manifest must name an explicit supported package configuration",
                ));
            }
        };
        let repo = root.join(".lake/packages").join(name);
        let git_dir = repo.join(".git");
        if !git_dir.symlink_metadata()?.is_dir() || git_dir.canonicalize()? != git_dir {
            return Err(invalid("expected a canonical materialized Git package"));
        }
        absent(&repo.join(alternate))?;
        if git(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])? != format!("{}\n", package.rev).as_bytes() {
            return Err(invalid(format!("locked package HEAD differs: {name}")));
        }
        let tracked = git(&repo, &["ls-tree", &package.rev, "--", &package.config_file])?;
        if !tracked.starts_with(b"100644 blob ") && !tracked.starts_with(b"100755 blob ") {
            return Err(invalid(format!(
                "package configuration is not a tracked regular file: {name}"
            )));
        }
        let object = format!("{}:{}", package.rev, package.config_file);
        let expected = git(&repo, &["cat-file", "blob", &object])?;
        if file_bytes(&repo.join(&package.config_file))? != expected {
            return Err(invalid(format!("locked package configuration changed: {name}")));
        }
        ordered.push(repo);
    }
    absent(&root.join(".lake/package-overrides.json"))?;
    Ok(ordered)
}

pub(super) fn check_dependency_free(root: &Path) -> io::Result<()> {
    absent(&root.join(".lake/package-overrides.json"))?;
    let manifest: Manifest = serde_json::from_slice(&file_bytes(&root.join("lake-manifest.json"))?)?;
    if manifest.packages_dir != ".lake/packages" || !manifest.packages.is_empty() {
        return Err(invalid("tooling workspace must have an empty dependency manifest"));
    }
    Ok(())
}

pub(super) fn validate_root_config(root: &Path, expected: &[u8; 32]) -> io::Result<()> {
    absent(&root.join("lakefile.toml"))?;
    let actual: [u8; 32] = Sha256::digest(file_bytes(&root.join("lakefile.lean"))?).into();
    if &actual != expected {
        return Err(invalid("admitted root Lake configuration changed"));
    }
    Ok(())
}

/// Check the exact admitted manifest and each explicitly selected package configuration
/// against locally available pinned Git objects while the existing writer holds custody.
///
/// # Errors
/// Refuses absent custody, overrides, unsupported layouts, and changed or missing inputs.
pub fn validate_pinned_lake_configs(root: &Path, expected_manifest: &[u8; 32]) -> io::Result<()> {
    let lock = lean_toolchain::shared_writer::project_lock_path(root)?;
    crate::shared_writer::verify_committed_writer_process(&lock)?;
    check(root, expected_manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_configs_refuse_dirty_package_and_overrides_without_lake() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let repo = root.join(".lake/packages/test-package");
        fs::create_dir_all(&repo)?;
        let setup = |args: &[&str]| -> io::Result<()> {
            assert!(
                Command::new("git")
                    .current_dir(&repo)
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "-c",
                        "commit.gpgsign=false",
                        "-c",
                        "core.hooksPath=/dev/null"
                    ])
                    .args(args)
                    .stdout(Stdio::null())
                    .status()?
                    .success()
            );
            Ok(())
        };
        let config = repo.join("lakefile.toml");
        let original = "name = \"test-package\"\n";
        setup(&["init", "--quiet"])?;
        fs::write(&config, original)?;
        setup(&["add", "lakefile.toml"])?;
        setup(&["commit", "--quiet", "-m", "fixture"])?;
        let head = String::from_utf8(git(&repo, &["rev-parse", "HEAD"])?).map_err(io::Error::other)?;
        let manifest = serde_json::to_vec(&serde_json::json!({
            "packagesDir":".lake/packages",
            "packages":[{"name":"«test-package»","type":"git","rev":head.trim(),"configFile":"lakefile.toml"}]
        }))?;
        fs::write(root.join("lake-manifest.json"), &manifest)?;
        let digest = Sha256::digest(&manifest).into();
        check(&root, &digest)?;
        fs::write(&config, "name = \"changed\"\n")?;
        assert!(
            check(&root, &digest)
                .unwrap_err()
                .to_string()
                .contains("configuration changed")
        );
        fs::write(config, original)?;
        fs::write(root.join(".lake/package-overrides.json"), "[]")?;
        assert!(
            check(&root, &digest)
                .unwrap_err()
                .to_string()
                .contains("package-overrides.json")
        );
        Ok(())
    }
}
