//! File setup from the selected Lake. Keep Lake's original frame, including
//! import visibility, plugin entry points and compiler options.
use super::{ProjectBroker, ProjectHint, Result, ServerError};
use crate::cancellation::CallCancellation;
use lean_toolchain::shared_writer::SharedWriter;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs, io,
    io::Read as _,
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

#[derive(Debug, Eq, PartialEq)]
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

fn parse_setup(bytes: &[u8]) -> io::Result<ModuleSetup> {
    if bytes.len() > crate::shared_writer::MODULE_SETUP_LIMIT {
        return Err(invalid("Lake module setup exceeds capture limit"));
    }
    let setup: ModuleSetup =
        serde_json::from_slice(bytes).map_err(|error| invalid(&format!("invalid Lake module setup: {error}")))?;
    if setup.name.is_empty() {
        return Err(invalid("missing Lake module name"));
    }
    Ok(setup)
}

fn stage(bytes: Vec<u8>, writer: &SharedWriter, sysroot: &Path, root: &Path, file: &Path) -> io::Result<StagedSetup> {
    let setup = parse_setup(&bytes)?;
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

/// Supplied only by the managed caller for its approved project. The standalone
/// writer validates the full request and reserves under its existing admission lock.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ChargedWriter {
    executable: PathBuf,
    image: Identity,
    inherited_temp: PathBuf,
    request: String,
}

impl ChargedWriter {
    pub(crate) fn new(root: &Path, executable: PathBuf, request: String) -> io::Result<Self> {
        if request.len() > 16 * 1024 {
            return Err(invalid("oversized module setup storage request"));
        }
        let value: serde_json::Value = serde_json::from_str(&request)?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(1)
            || value
                .get("budget_bytes")
                .and_then(serde_json::Value::as_u64)
                .is_none_or(|v| v == 0)
        {
            return Err(invalid("module setup requires an explicit v1 full-turn storage budget"));
        }
        // The standalone writer owns full root validation and reservation under its admission lock.
        // Bind this startup approval to the project before the helper can see the request.
        let roots = value
            .get("roots")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid("module setup storage request lacks roots"))?;
        if roots.len() > 48
            || roots.iter().any(|r| {
                r["role"].as_str().is_none()
                    || !r["path"].as_str().is_some_and(|s| Path::new(s).is_absolute())
                    || r["device"].as_u64().is_none()
                    || r["inode"].as_u64().is_none()
            })
        {
            return Err(invalid(
                "module setup requires bounded storage roots with directory identities",
            ));
        }
        if value.get("floor_bytes").and_then(serde_json::Value::as_u64).is_none()
            || !roots
                .iter()
                .any(|r| r["role"] == "project" && r["path"].as_str().map(Path::new) == Some(root))
            || !["registry", "scratch_parent"]
                .iter()
                .all(|role| roots.iter().any(|r| r["role"].as_str() == Some(role)))
        {
            return Err(invalid(
                "module setup requires the approved project, registry, scratch parent and floor",
            ));
        }
        // The setup command inherits the helper's temporary directory. Its v1 command
        // envelope permits the writer-owned cache selector, but no TMPDIR override.
        let temp = std::env::temp_dir().canonicalize()?;
        if !roots
            .iter()
            .any(|r| r["path"].as_str().map(Path::new) == Some(temp.as_path()))
        {
            return Err(invalid(
                "module setup storage request must include the inherited temporary directory",
            ));
        }
        // This host sends no source handoff; its caller owns the read and explicit close.
        if value.get("command_only").is_some_and(|v| v.as_bool() != Some(false))
            || value.get("handoff_sha256").is_some()
            || value.get("approval_generation").is_some()
        {
            return Err(invalid("unsupported module setup storage handoff"));
        }
        let image = Identity::read(&executable)?;
        Ok(Self {
            executable,
            image,
            inherited_temp: temp,
            request,
        })
    }

    async fn qualify(&self, root: &Path, timeout: Duration) -> io::Result<Identity> {
        use tokio::io::AsyncReadExt as _;
        const LIMIT: usize = 16 * 1024;
        if std::env::temp_dir().canonicalize()? != self.inherited_temp {
            return Err(invalid("module setup temporary directory changed"));
        }
        let image = Identity::read(&self.executable)?;
        if image != self.image {
            return Err(invalid("configured module setup writer changed"));
        }
        let lock = lean_toolchain::shared_writer::project_lock_path(root)?;
        tokio::time::timeout(timeout, async {
            use std::process::Stdio;
            let mut child = tokio::process::Command::new(&self.executable)
                .arg("writer-lock-path")
                .arg(root)
                .arg("--capabilities")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or_else(|| invalid("writer descriptor unavailable"))?
                .take((LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > LIMIT || !child.wait().await?.success() {
                return Err(invalid("writer did not provide its bounded capability descriptor"));
            }
            check_capability(&bytes, root, &lock)?;
            Ok(image)
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn check_capability(bytes: &[u8], root: &Path, lock: &Path) -> io::Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let capabilities = value.get("capabilities").and_then(serde_json::Value::as_array);
    if value.get("protocol").and_then(serde_json::Value::as_u64) != Some(1)
        || value.get("project_root").and_then(serde_json::Value::as_str)
            != Some(root.to_str().ok_or_else(|| invalid("non-UTF-8 writer root"))?)
        || value.get("lock_path").and_then(serde_json::Value::as_str)
            != Some(lock.to_str().ok_or_else(|| invalid("non-UTF-8 writer lock"))?)
        || !capabilities.is_some_and(|caps| caps.iter().any(|v| v == "lake_module_setup_capture_v1"))
    {
        return Err(invalid(
            "expected lake_module_setup_capture_v1 for the selected project lock",
        ));
    }
    Ok(())
}

fn run_setup(
    writer: &mut SharedWriter,
    root: &Path,
    file: &Path,
    source: &str,
    timeout: Duration,
    started: Instant,
    cancel: &CallCancellation,
) -> Result<StagedSetup> {
    if !file.starts_with(root)
        || file.canonicalize()? != file
        || file.extension().is_none_or(|s| s != "lean")
        || fs::read(file)? != source.as_bytes()
    {
        return Err(ServerError::BadProject(
            "current source differs from selected verification file".into(),
        ));
    }
    let id = crate::toolchain::ToolchainId::from_lake_root(root).map_err(|e| ServerError::BadProject(e.to_string()))?;
    let sysroot = id
        .elan_dir()
        .map_err(|e| ServerError::BadProject(e.to_string()))?
        .canonicalize()?;
    let mut command = Command::new(sysroot.join("bin/lake"));
    command
        .current_dir(root)
        .args(["--no-build", "--no-cache", "setup-file"])
        .arg(file)
        .env("LAKE_CACHE_DIR", writer.asset_dir())
        .env("LAKE_ARTIFACT_CACHE", "false")
        .env(crate::shared_writer::MODULE_SETUP_ENV, "1");
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
    let path = writer.asset_dir().join(crate::shared_writer::MODULE_SETUP_FILE);
    let staged = fs::File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let meta = staged.metadata()?;
    let limit = crate::shared_writer::MODULE_SETUP_LIMIT as u64;
    if !meta.is_file() || meta.len() > limit {
        return Err(invalid("invalid Lake module setup staging file").into());
    }
    let mut bytes = Vec::new();
    staged.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() > crate::shared_writer::MODULE_SETUP_LIMIT
        || crate::shared_writer::module_setup_digest(&bytes).as_bytes() != result.stdout
    {
        return Err(invalid("Lake module setup exceeds limit or differs from writer receipt").into());
    }
    Ok(stage(bytes, writer, &sysroot, root, file)?)
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
        let selected = match self.runtime_config.module_setup_writer.clone() {
            Some(selection) => {
                let started = Instant::now();
                let image = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "module setup cancelled before writer admission").into()),
                    result = selection.qualify(&root, timeout) => result?,
                };
                Some((selection, image, started))
            }
            None => None,
        };
        let mut on_drop = cancel.cancel_on_drop();
        let result = tokio::task::spawn_blocking(move || -> Result<StagedSetup> {
            let _activity = activity;
            let cancelled = || cancel.is_cancelled();
            let (mut writer, started) = if let Some((selection, image, started)) = &selected {
                if Identity::read(&selection.executable)? != *image {
                    return Err(invalid("selected module setup writer changed before admission").into());
                }
                let writer = SharedWriter::open_with_storage_request(
                    &selection.executable,
                    &root,
                    &[],
                    &selection.request,
                    timeout.saturating_sub(started.elapsed()),
                    &cancelled,
                )?;
                (writer, *started)
            } else {
                let helper = crate::shared_writer::contract()?.executable;
                let started = Instant::now();
                (
                    SharedWriter::open_cancellable(&helper, &root, timeout, &cancelled)?,
                    started,
                )
            };
            let outcome = run_setup(&mut writer, &root, &file, &source, timeout, started, &cancel);
            if selected.is_some()
                && let Err(close) = writer.close(timeout.saturating_sub(started.elapsed()))
            {
                return Err(match outcome {
                    Ok(_) => ServerError::Internal(format!("module setup writer settlement unconfirmed: {close}")),
                    Err(primary) => {
                        ServerError::BadProject(format!("{primary}; writer settlement unconfirmed: {close}"))
                    }
                });
            }
            outcome
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
    fn setup_parser_requires_complete_bounded_typed_json() {
        assert!(parse_setup(br#"{"name":"M"}"#).is_err());
        assert!(parse_setup(br#"{"name":"M""#).is_err());
    }

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

#[cfg(test)]
mod charged_tests;
