//! File setup from the selected Lake. Keep Lake's original frame, including
//! import visibility, plugin entry points and compiler options.
use super::{ProjectBroker, ProjectHint, Result, ServerError};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const SETUP_FILE: &str = "Lake setup-file";

/// Lake's exit code when `--no-build` meets a target it would have to rebuild
/// (`noBuildCode`, `Lake/Build/Run.lean`).
const NO_BUILD_EXIT: i32 = 3;

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// The setup Lake printed, or [`ServerError::OutOfDate`] naming the targets
/// Lake would have to rebuild first.
fn setup_output(output: crate::lake_options::ReaderOutput) -> Result<Vec<u8>> {
    if output.exit_code != NO_BUILD_EXIT {
        return Ok(output.stdout);
    }
    // `reportResult` (`Lake/Build/Run.lean`) prints this line, then one
    // `- NAME` line per failed job.
    let targets = output
        .stderr
        .lines()
        .skip_while(|line| *line != "Some required targets logged failures:")
        .skip(1)
        .map_while(|line| line.strip_prefix("- "))
        .map(str::to_owned)
        .collect();
    Err(ServerError::OutOfDate {
        targets,
        output: output.stderr,
    })
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

#[derive(Debug, Eq, PartialEq, serde::Serialize)]
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

pub(crate) struct StagedSetup {
    pub(crate) bytes: Vec<u8>,
    pub(crate) options: BTreeMap<String, lean_toolchain::LeanOptionValue>,
    sysroot: PathBuf,
    domains: Vec<PathBuf>,
    inputs: BTreeMap<PathBuf, Identity>,
}

impl StagedSetup {
    pub(super) fn artifact_identity_sha256(&self, shim_digest: &str) -> io::Result<String> {
        let executable = Identity::read(&self.sysroot.join("bin/lean"))?;
        let bytes = serde_json::to_vec(&(&self.sysroot, executable, &self.domains, &self.inputs, shim_digest))?;
        Ok(crate::shared_writer::module_setup_digest(&bytes))
    }

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
    if bytes.len() > crate::native_reader::PROGRAM_OUTPUT_LIMIT {
        return Err(invalid("Lake module setup exceeds capture limit"));
    }
    let setup: ModuleSetup =
        serde_json::from_slice(bytes).map_err(|error| invalid(&format!("invalid Lake module setup: {error}")))?;
    if setup.name.is_empty() {
        return Err(invalid("missing Lake module name"));
    }
    Ok(setup)
}

pub(crate) fn stage(
    bytes: Vec<u8>,
    domains: &[PathBuf],
    sysroot: &Path,
    root: &Path,
    file: &Path,
) -> io::Result<StagedSetup> {
    let setup = parse_setup(&bytes)?;
    // These fields pass through byte-for-byte. Only Lake/Lean interprets them.
    let release_lib = sysroot.join("lib");
    let mut inputs = BTreeMap::new();
    let mut validate = |p: &Path| -> io::Result<()> {
        if !p.is_absolute()
            || p.canonicalize()? != p
            || !p.is_file()
            || !(p.starts_with(&release_lib) || domains.iter().any(|r| p.starts_with(r.join(".lake/build"))))
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
        domains: domains.to_vec(),
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
        let root = self.resolve(hint)?;
        if root != meta.canonical_root {
            return Err(ServerError::BadProject("project selection changed".into()));
        }
        // The turn and guard G hold from setup-file until the check ends.
        self.ensure_read(&root).await?;
        let setup = self.setup_file(&root, Path::new(file), source).await?;
        let activity = self.preparation_activity(&root).await?;
        let reader = self.installed_reader(&root, meta).await?;
        setup.check(&root, &reader.sysroot, &reader.custody)?;
        if fs::read(file)? != source.as_bytes() {
            return Err(ServerError::BadProject("source changed during setup transfer".into()));
        }
        Ok((setup, reader, activity))
    }

    /// The module Lake names for `file`, and the `.olean` of every module it
    /// imports, directly or not, as `lake setup-file` reports them under this
    /// request's read turn.
    pub(crate) async fn module_imports(
        &self,
        root: &Path,
        file: &Path,
        source: &str,
    ) -> Result<(String, Vec<(String, PathBuf)>)> {
        self.ensure_read(root).await?;
        let setup = parse_setup(&self.setup_file(root, file, source).await?.bytes)?;
        let imports = setup
            .import_arts
            .into_iter()
            .map(|(module, parts)| {
                let olean = parts
                    .first()
                    .and_then(|oleans| {
                        oleans
                            .iter()
                            .find(|path| path.extension().is_some_and(|ext| ext == "olean"))
                    })
                    .cloned()
                    .ok_or_else(|| ServerError::BadProject(format!("Lake reported no .olean for import {module}")))?;
                Ok((module, olean))
            })
            .collect::<Result<_>>()?;
        Ok((setup.name, imports))
    }

    /// Run `lake setup-file` as a native reader. The guardian holds the project
    /// lock shared while Lake reads the build products and prints the setup.
    /// No step takes the lock exclusively. When an import is out of date, Lake
    /// builds nothing and this returns [`ServerError::OutOfDate`].
    async fn setup_file(&self, root: &Path, file: &Path, source: &str) -> Result<StagedSetup> {
        if !file.starts_with(root)
            || file.canonicalize()? != file
            || file.extension().is_none_or(|s| s != "lean")
            || fs::read(file)? != source.as_bytes()
        {
            return Err(ServerError::BadProject(
                "current source differs from selected verification file".into(),
            ));
        }
        let id =
            crate::toolchain::ToolchainId::from_lake_root(root).map_err(|e| ServerError::BadProject(e.to_string()))?;
        if id.window_verdict() != crate::toolchain::WindowVerdict::Supported {
            return Err(ServerError::BadProject(format!(
                "{SETUP_FILE} needs a supported toolchain, not {id}"
            )));
        }
        let sysroot = id
            .elan_dir()
            .map_err(|e| ServerError::BadProject(e.to_string()))?
            .canonicalize()?;
        let domains = lean_toolchain::shared_writer::project_artifact_roots(root)?;
        let activity = self.preparation_activity(root).await?;
        let custody = crate::artifact_reads::ArtifactReadGuard::try_acquire(root)?;
        let lake = sysroot.join("bin/lake");
        let (cwd, source_file) = (root.to_owned(), file.to_owned());
        let output = crate::lake_options::reader_run(
            SETUP_FILE,
            root.to_owned(),
            crate::cancellation::CallCancellation::current()
                .remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.config.max_workers.get(),
            custody,
            None,
            Some(Box::new(activity)),
            &[0, NO_BUILD_EXIT],
            move |assets| {
                // Point any Lake cache at the reader's private inputs.
                let cache = assets.join("lake-cache");
                fs::create_dir(&cache)?;
                Ok(crate::native_reader::NativeReaderProgram {
                    command: vec![
                        lake.into_os_string(),
                        "--no-build".into(),
                        "--no-cache".into(),
                        "setup-file".into(),
                        source_file.into_os_string(),
                    ],
                    environment: vec![
                        ("LAKE_CACHE_DIR".into(), cache.into_os_string()),
                        ("LAKE_ARTIFACT_CACHE".into(), "false".into()),
                    ],
                    cwd,
                    artifact_roots: Vec::new(),
                    max_stdout_bytes: crate::native_reader::PROGRAM_OUTPUT_LIMIT,
                    stdin: None,
                })
            },
        )
        .await
        .map_err(|failure| failure.error)?;
        Ok(stage(setup_output(output)?, &domains, &sysroot, root, file)?)
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
    fn no_build_exit_names_the_targets_lake_lists() {
        // What Lake 4.36 prints for `lake --no-build setup-file B.lean` when
        // B imports an unbuilt A.
        let stderr = "info: toolchain not updated; already up-to-date\n\
                      ✖ [2/4] Building A\n\
                      error: target is out-of-date and needs to be rebuilt\n\
                      Some required targets logged failures:\n\
                      - A\n";
        let output = |exit_code| crate::lake_options::ReaderOutput {
            exit_code,
            stdout: b"{}".to_vec(),
            stderr: stderr.to_owned(),
        };
        assert!(matches!(
            setup_output(output(NO_BUILD_EXIT)),
            Err(ServerError::OutOfDate { targets, output }) if targets == ["A"] && output == stderr
        ));
        assert!(matches!(setup_output(output(0)), Ok(bytes) if bytes == b"{}"));
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
            let guard = crate::artifact_reads::ArtifactReadGuard::try_acquire(&root)?;
            setup.check(&root, &setup.sysroot, &guard)?;
        }
        let replacement = output.join("replacement");
        fs::write(&replacement, b"same bytes")?;
        fs::rename(replacement, leaf)?;
        let guard = crate::artifact_reads::ArtifactReadGuard::try_acquire(&root)?;
        assert!(
            matches!(setup.check(&root, &setup.sysroot, &guard), Err(error) if error.kind() == io::ErrorKind::InvalidData)
        );
        Ok(())
    }
}
