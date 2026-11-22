//! Remove only the canonical first-party Lake build tree.
use std::{
    fs, io,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub(crate) const OPERATION: &str = "lake_first_party_clean_v1";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u8,
    stage: String,
    removed: bool,
}

impl Receipt {
    pub(crate) fn parse(bytes: &[u8]) -> io::Result<Self> {
        let value: Self = serde_json::from_slice(bytes)?;
        if value.version != 1 || value.stage != "first-party-clean" {
            return Err(io::Error::other("invalid first-party clean receipt"));
        }
        Ok(value)
    }
}

pub(crate) fn check(root: &Path, expected: &[u8; 32]) -> io::Result<PathBuf> {
    let config = super::whole_lint::read_checked_config(root, expected)?;
    let config = std::str::from_utf8(&config).map_err(io::Error::other)?;
    let supported = config.lines().map(str::trim).any(|line| line == "package Proofs where")
        && !["buildDir", "packagesDir", "lakeDir"]
            .iter()
            .any(|key| config.contains(key));
    if !supported {
        return Err(io::Error::other(
            "first-party clean requires canonical Proofs output layout",
        ));
    }

    let lake = root.join(".lake");
    let build = lake.join("build");
    for path in [&lake, &build] {
        match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() && path.canonicalize()? == *path => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Ok(_) => return Err(io::Error::other("first-party clean output path is aliased")),
            Err(error) => return Err(error),
        }
    }
    for dependency in lean_toolchain::shared_writer::project_artifact_roots(root)? {
        if dependency != root && (dependency.starts_with(&build) || build.starts_with(&dependency)) {
            return Err(io::Error::other("first-party clean output overlaps dependency custody"));
        }
    }
    Ok(build)
}

fn check_tree(path: &Path, device: u64) -> io::Result<()> {
    for entry in path.read_dir()? {
        let path = entry?.path();
        let metadata = path.symlink_metadata()?;
        if metadata.is_dir() {
            if metadata.dev() != device {
                return Err(io::Error::other("first-party clean refuses mounted outputs"));
            }
            check_tree(&path, device)?;
        }
    }
    Ok(())
}

pub(crate) fn run(
    root: &Path,
    expected: &[u8; 32],
    authorize: impl Fn() -> io::Result<std::time::Duration>,
) -> io::Result<()> {
    let build = check(root, expected)?;
    let removed = match build.symlink_metadata() {
        Ok(metadata) => {
            let device = root.metadata()?.dev();
            if metadata.dev() != device {
                return Err(io::Error::other("first-party clean refuses mounted outputs"));
            }
            check_tree(&build, device)?;
            check(root, expected)?;
            let lake = root.join(".lake");
            let parent = fs::File::from(rustix::fs::open(
                &lake,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )?);
            let opened = parent.metadata()?;
            let named = lake.symlink_metadata()?;
            if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) || opened.dev() != device {
                return Err(io::Error::other("first-party clean parent changed"));
            }
            authorize()?;
            // This fixed writer subprocess owns its cwd. Keep deletion relative
            // to the opened parent even if an outside actor renames it.
            rustix::process::fchdir(&parent)?;
            let relative = Path::new("build");
            let observed = relative.symlink_metadata()?;
            if (observed.dev(), observed.ino()) != (metadata.dev(), metadata.ino()) {
                return Err(io::Error::other("first-party clean output changed"));
            }
            fs::remove_dir_all(relative)?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    authorize()?;
    println!(
        "{}",
        serde_json::to_string(&Receipt {
            version: 1,
            stage: "first-party-clean".into(),
            removed,
        })?
    );
    Ok(())
}
