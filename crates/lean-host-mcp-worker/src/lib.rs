//! The deployment record that `lean-host-mcp install` writes beside each
//! worker it installs.
//!
//! The record is `worker.json` in the worker's generation directory. Its
//! `deployment` part names what the generation runs: the worker binary's
//! digest, the Lean prefix, the shims and the digest of each file the worker
//! loads. The server writes the record and checks it before it starts the
//! worker; the worker checks itself against the same record before it serves.
//! The server owns the rest of the record.
//!
//! This library links no Lean. The server depends on it with default features
//! off, which leaves out the worker binary and its Lean runtime.

use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{self, Read as _},
    path::{Component, Path, PathBuf},
};

/// File name of the record in a worker generation.
pub const RECORD_FILE_NAME: &str = "worker.json";

/// Directory of a generation that holds its own built copy of the SDK shims.
pub const SHIMS_DIR: &str = "shims";

/// Largest record the server or the worker reads.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

/// What one installed generation runs, and the digest of each file it loads.
///
/// In the record, each path inside the generation is relative to it and
/// starts with [`SHIMS_DIR`]; paths under the Lean prefix stay absolute.
/// [`Deployment::read`] and [`Deployment::resolve_in`] make them absolute.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Deployment {
    /// SHA-256 of the worker binary, in lowercase hex.
    pub worker_sha256: String,
    /// The Lean prefix the worker was built against and runs with.
    pub sysroot: PathBuf,
    /// The generation's shims, as the SDK loads them.
    pub prepared: LeanWorkerPreparedHostArtifacts,
    /// SHA-256 of each file the worker loads: the shim manifest, images and
    /// compiled modules, and the Lean release files it binds.
    pub files: BTreeMap<PathBuf, String>,
}

impl Deployment {
    /// The deployment in the record of `generation`, with its paths resolved
    /// there.
    ///
    /// # Errors
    ///
    /// Fails when the record is missing or larger than 1 MiB, when it is not
    /// JSON with a `deployment`, or when [`Self::resolve_in`] refuses a path.
    pub fn read(generation: &Path) -> io::Result<Self> {
        #[derive(Deserialize)]
        struct Record {
            deployment: Deployment,
        }
        let mut record: Record = serde_json::from_slice(&read_record(generation)?)?;
        record.deployment.resolve_in(generation)?;
        Ok(record.deployment)
    }

    /// Make each generation path absolute under `generation`.
    ///
    /// # Errors
    ///
    /// Fails on a path that does not start with [`SHIMS_DIR`], or that holds
    /// a root, a prefix, `.` or `..`.
    pub fn resolve_in(&mut self, generation: &Path) -> io::Result<()> {
        self.map_generation_paths(|path| {
            if !path.starts_with(SHIMS_DIR) || path.components().any(|c| !matches!(c, Component::Normal(_))) {
                return Err(invalid_data("invalid installed generation-relative path"));
            }
            Ok(generation.join(path))
        })
    }

    /// Make each path inside `generation` relative to it, as the record
    /// stores them.
    ///
    /// # Errors
    ///
    /// Fails on a shim path outside `generation`.
    pub fn relative_to(&mut self, generation: &Path) -> io::Result<()> {
        self.map_generation_paths(|path| {
            path.strip_prefix(generation)
                .map(Path::to_path_buf)
                .map_err(|_| invalid_data("shim path outside its generation"))
        })
    }

    /// Rewrite the shim manifest path, the shim roots and each recorded file
    /// outside the Lean prefix with `map`.
    fn map_generation_paths(&mut self, map: impl Fn(&Path) -> io::Result<PathBuf>) -> io::Result<()> {
        let string = |value: &mut String| -> io::Result<()> {
            *value = map(Path::new(value))?
                .into_os_string()
                .into_string()
                .map_err(|_| invalid_data("invalid installed path encoding"))?;
            Ok(())
        };
        let prepared = &mut self.prepared;
        string(&mut prepared.shim_manifest_path)?;
        for path in &mut prepared.shim_search_paths {
            string(path)?;
        }
        let sysroot = &self.sysroot;
        self.files = std::mem::take(&mut self.files)
            .into_iter()
            .map(|(path, hash)| {
                if path.starts_with(sysroot) {
                    Ok((path, hash))
                } else {
                    Ok((map(&path)?, hash))
                }
            })
            .collect::<io::Result<_>>()?;
        Ok(())
    }
}

/// The bytes of the record in `generation`.
///
/// # Errors
///
/// Fails when the record cannot be read or is larger than 1 MiB.
pub fn read_record(generation: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(generation.join(RECORD_FILE_NAME))?
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(invalid_data)? > MAX_RECORD_BYTES {
        return Err(invalid_data("oversized installed record"));
    }
    Ok(bytes)
}

/// An [`io::ErrorKind::InvalidData`] error saying `message`: a file or a
/// record that does not hold what it must.
pub fn invalid_data(message: impl fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(sysroot: &Path, shims: &Path) -> Deployment {
        Deployment {
            worker_sha256: "0".repeat(64),
            sysroot: sysroot.to_path_buf(),
            prepared: LeanWorkerPreparedHostArtifacts {
                protocol: 1,
                shim_manifest_path: shims.join("manifest.json").to_string_lossy().into_owned(),
                shim_manifest_json: "{}".into(),
                shim_search_paths: vec![shims.join("lib").to_string_lossy().into_owned()],
            },
            files: BTreeMap::from([
                (shims.join("lib/A.olean"), "a".into()),
                (sysroot.join("lib/lean/Init.olean"), "init".into()),
            ]),
        }
    }

    #[test]
    fn the_record_stores_generation_paths_relative_and_read_resolves_them() -> io::Result<()> {
        let generation = tempfile::tempdir()?;
        let sysroot = Path::new("/lean");
        let absolute = deployment(sysroot, &generation.path().join(SHIMS_DIR));
        let mut stored = absolute.clone();
        stored.relative_to(generation.path())?;
        assert_eq!(stored, deployment(sysroot, Path::new(SHIMS_DIR)));
        let record = serde_json::json!({ "deployment": stored, "smoke": null });
        fs::write(generation.path().join(RECORD_FILE_NAME), serde_json::to_vec(&record)?)?;
        assert_eq!(Deployment::read(generation.path())?, absolute);
        Ok(())
    }

    #[test]
    fn resolve_in_refuses_a_path_that_leaves_the_shims() {
        for path in ["lib", "shims/../worker", "/shims/lib", "./shims/lib"] {
            let mut stored = deployment(Path::new("/lean"), Path::new(SHIMS_DIR));
            stored.prepared.shim_search_paths = vec![path.to_owned()];
            let refused = stored.resolve_in(Path::new("/generation"));
            assert!(
                refused.is_err_and(|error| error.kind() == io::ErrorKind::InvalidData),
                "{path}"
            );
        }
    }

    #[test]
    fn read_refuses_a_record_over_one_mebibyte() -> io::Result<()> {
        let generation = tempfile::tempdir()?;
        let mut record = vec![b' '; usize::try_from(MAX_RECORD_BYTES).map_err(invalid_data)?];
        fs::write(generation.path().join(RECORD_FILE_NAME), &record)?;
        assert!(read_record(generation.path()).is_ok());
        record.push(b' ');
        fs::write(generation.path().join(RECORD_FILE_NAME), &record)?;
        let refused = read_record(generation.path());
        assert!(refused.is_err_and(|error| error.kind() == io::ErrorKind::InvalidData));
        Ok(())
    }
}
