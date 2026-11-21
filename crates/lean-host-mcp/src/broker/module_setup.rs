//! File setup from the selected Lake. Keep Lake's original frame, including
//! import visibility, plugin entry points and compiler options.
use super::{ProjectBroker, ProjectHint, Result, ServerError};
use crate::cancellation::CallCancellation;
use lean_toolchain::shared_writer::SharedWriter;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModuleSetup {
    name: String,
    #[serde(rename = "package")]
    _package: Option<String>,
    #[serde(rename = "isModule")]
    _is_module: bool,
    #[serde(rename = "imports")]
    _imports: Option<Vec<serde_json::Value>>,
    import_arts: BTreeMap<String, Vec<Vec<PathBuf>>>,
    plugins: Vec<Plugin>,
    dynlibs: Vec<PathBuf>,
    options: BTreeMap<String, lean_toolchain::LeanOptionValue>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Plugin {
    Path(PathBuf),
    Named {
        path: PathBuf,
        #[serde(rename = "initFn")]
        init_fn: Option<String>,
    },
}

#[derive(Eq, PartialEq)]
struct Identity(u64, u64, u64, i64, i64);

impl Identity {
    fn read(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let m = fs::symlink_metadata(path)?;
        if !m.is_file() || path.canonicalize()? != path {
            return Err(invalid("setup file no longer canonical"));
        }
        Ok(Self(m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec()))
    }
}

pub(super) struct StagedSetup {
    pub(super) bytes: Vec<u8>,
    pub(super) options: BTreeMap<String, lean_toolchain::LeanOptionValue>,
    sysroot: PathBuf,
    domains: Vec<PathBuf>,
    inputs: BTreeMap<PathBuf, Identity>,
}

impl StagedSetup {
    pub(super) fn check(
        &self,
        root: &Path,
        sysroot: &Path,
        custody: &crate::artifact_reads::ArtifactReadGuard,
    ) -> io::Result<()> {
        if self.sysroot != sysroot || self.domains != lean_toolchain::shared_writer::project_artifact_roots(root)? {
            return Err(invalid("setup domains or toolchain changed during reader transfer"));
        }
        for (p, identity) in &self.inputs {
            if p != &root.join("lean-toolchain")
                && !p.starts_with(sysroot)
                && !p.starts_with(root)
                && !custody.covers_artifact_root(p)
            {
                return Err(invalid("setup path outside reader custody"));
            }
            if Identity::read(p)? != *identity {
                return Err(invalid("setup input changed during reader transfer"));
            }
        }
        Ok(())
    }
}

fn stage(bytes: Vec<u8>, writer: &SharedWriter, sysroot: &Path, root: &Path, file: &Path) -> io::Result<StagedSetup> {
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("Lake module setup exceeds capture limit"));
    }
    let setup: ModuleSetup = serde_json::from_slice(&bytes)?;
    if setup.name.is_empty() {
        return Err(invalid("missing Lake module name"));
    }
    // These fields pass through byte-for-byte. Only Lake/Lean interprets them.
    let release_lib = sysroot.join("lib");
    let mut inputs = BTreeMap::new();
    let mut validate = |p: &Path| -> io::Result<()> {
        if !p.is_absolute()
            || p.canonicalize()? != p
            || !p.is_file()
            || !(p.starts_with(&release_lib)
                || writer
                    .write_roots()
                    .iter()
                    .any(|r| p.starts_with(r.join(".lake/build"))))
        {
            return Err(invalid(
                "Lake setup artifact outside held project outputs or selected release",
            ));
        }
        inputs.insert(p.to_owned(), Identity::read(p)?);
        Ok(())
    };
    for parts in setup.import_arts.values() {
        if parts.is_empty()
            || parts.len() > 2
            || parts.first().is_none_or(|oleans| oleans.is_empty() || oleans.len() > 3)
            || parts.get(1).is_some_and(|ir| ir.len() > 2)
        {
            return Err(invalid("unsupported Lake import artifact layout"));
        }
        for path in parts.iter().flatten() {
            validate(path)?;
        }
    }
    for path in &setup.dynlibs {
        validate(path)?;
    }
    for plugin in &setup.plugins {
        match plugin {
            Plugin::Path(p) => validate(p)?,
            Plugin::Named { path, init_fn } => {
                if init_fn.as_ref().is_some_and(String::is_empty) {
                    return Err(invalid("empty plugin initializer"));
                }
                validate(path)?;
            }
        }
    }
    for path in [
        file.to_owned(),
        root.join("lean-toolchain"),
        root.join("lake-manifest.json"),
    ] {
        inputs.insert(path.clone(), Identity::read(&path)?);
    }
    Ok(StagedSetup {
        bytes,
        options: setup.options,
        inputs,
        domains: writer.write_roots().to_vec(),
        sysroot: sysroot.to_owned(),
    })
}

impl ProjectBroker {
    pub(super) async fn file_reader_with_setup(
        &self,
        hint: &ProjectHint,
        meta: &crate::lake_meta::LakeProjectMeta,
        file: &str,
        source: &str,
    ) -> Result<(
        StagedSetup,
        super::verify_file::PreparedReader,
        super::control::PreparationActivityGuard,
    )> {
        let root = self.native_root(hint)?;
        if root != meta.canonical_root {
            return Err(ServerError::BadProject("project selection changed".into()));
        }
        let setup = self.prepare_file_setup(hint, file, source).await?;
        let activity = self.preparation_activity(&root).await?;
        let reader = Self::installed_reader(&root, meta)?;
        setup.check(&root, &reader.sysroot, &reader.custody)?;
        if fs::read(file)? != source.as_bytes() {
            return Err(ServerError::BadProject("source changed during setup transfer".into()));
        }
        Ok((setup, reader, activity))
    }

    async fn prepare_file_setup(&self, hint: &ProjectHint, file: &str, source: &str) -> Result<StagedSetup> {
        let root = self.native_root(hint)?;
        let activity = self.preparation_activity(&root).await?;
        let timeout = Duration::from_millis(self.request_timeout_millis());
        let file = PathBuf::from(file);
        let source = source.to_owned();
        let cancel = CallCancellation::current();
        let mut on_drop = cancel.cancel_on_drop();
        let result = tokio::task::spawn_blocking(move || -> Result<StagedSetup> {
            let _activity = activity;
            let helper = crate::shared_writer::contract()?.executable;
            let started = Instant::now();
            let mut writer = SharedWriter::open_cancellable(&helper, &root, timeout, &|| cancel.is_cancelled())?;
            if !file.starts_with(&root)
                || file.canonicalize()? != file
                || file.extension().is_none_or(|s| s != "lean")
                || fs::read(&file)? != source.as_bytes()
            {
                return Err(ServerError::BadProject(
                    "current source differs from selected verification file".into(),
                ));
            }
            let id = crate::toolchain::ToolchainId::from_lake_root(&root)
                .map_err(|e| ServerError::BadProject(e.to_string()))?;
            let sysroot = id
                .elan_dir()
                .map_err(|e| ServerError::BadProject(e.to_string()))?
                .canonicalize()?;
            let mut command = Command::new(sysroot.join("bin/lake"));
            command
                .current_dir(&root)
                .args(["--no-build", "--no-cache", "setup-file"])
                .arg(&file)
                .env("LAKE_CACHE_DIR", writer.asset_dir())
                .env("LAKE_ARTIFACT_CACHE", "false");
            let result = writer.run_cancellable(&command, timeout.saturating_sub(started.elapsed()), &|| {
                cancel.is_cancelled()
            })?;
            if !result.status.success() {
                return Err(ServerError::BadProject(format!(
                    "Lake setup-file failed ({}): {}",
                    result.status,
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
            Ok(stage(result.stdout, &writer, &sysroot, &root, &file)?)
        })
        .await;
        on_drop.disarm();
        result.map_err(|e| ServerError::Internal(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_refuses_replaced_artifact_even_with_identical_bytes() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let output = root.join(".lake/build/lib/lean");
        fs::create_dir_all(&output)?;
        let leaf = output.join("Dep.olean");
        fs::write(&leaf, b"same bytes")?;
        let setup = StagedSetup {
            bytes: Vec::new(),
            options: BTreeMap::new(),
            sysroot: root.join("release"),
            domains: lean_toolchain::shared_writer::project_artifact_roots(&root)?,
            inputs: BTreeMap::from([(leaf.clone(), Identity::read(&leaf)?)]),
        };
        {
            let guard = crate::artifact_reads::ArtifactReadGuard::for_project(&root)?;
            setup.check(&root, &setup.sysroot, &guard)?;
        }
        let replacement = output.join("replacement");
        fs::write(&replacement, b"same bytes")?;
        fs::rename(replacement, leaf)?;
        let guard = crate::artifact_reads::ArtifactReadGuard::for_project(&root)?;
        assert!(
            matches!(setup.check(&root, &setup.sysroot, &guard), Err(error) if error.kind() == io::ErrorKind::InvalidData)
        );
        Ok(())
    }
}
