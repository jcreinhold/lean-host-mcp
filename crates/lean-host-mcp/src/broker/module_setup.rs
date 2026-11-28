//! File setup and module sources from the selected Lake. Keep Lake's original
//! frame, including import visibility, plugin entry points and compiler
//! options.
use super::{ProjectBroker, ProjectHint, Result, ServerError};
use crate::cancellation::CallCancellation;
use crate::envelope::TargetCheckNotStarted;
use crate::error::{invalid, invalid_data};
use lean_toolchain::LeanOptionValue;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const SETUP_FILE: &str = "Lake setup-file";
const MODULE_SOURCES: &str = "Lake module sources";
const MODULE_LOOKUP: &str = "Lake module lookup";
const OPTION_DISCOVERY: &str = "Lake option discovery";

/// The header `lake setup-file` reads in place of the file's own when it
/// looks up options. It imports nothing, so Lake builds and checks no import,
/// and a stale import cannot fail the lookup.
const NO_IMPORTS: &str = r#"{"imports":[],"isModule":false}"#;

/// Lake's exit code when `--no-build` meets a target it would have to rebuild
/// (`noBuildCode`, `Lake/Build/Run.lean`).
const NO_BUILD_EXIT: i32 = 3;

/// Lake's exit code when a job or a target name fails.
const FAILURE_EXIT: i32 = 1;

/// The most names one failure looks up, one `lake query` each, before it
/// gives up on telling a typo from an unbuilt module.
const LOOKUP_LIMIT: usize = 8;

/// The names of the jobs that failed: `reportResult` (`Lake/Build/Run.lean`)
/// prints this line, then one `- NAME` line per failed job.
fn failed_jobs(stderr: &str) -> impl Iterator<Item = &str> {
    stderr
        .lines()
        .skip_while(|line| *line != "Some required targets logged failures:")
        .skip(1)
        .map_while(|line| line.strip_prefix("- "))
}

/// The setup Lake printed, or [`ServerError::OutOfDate`] naming the targets
/// Lake would have to rebuild first.
fn setup_output(output: super::reader::ReaderOutput) -> Result<Vec<u8>> {
    if output.exit_code != NO_BUILD_EXIT {
        return Ok(output.stdout);
    }
    Err(ServerError::OutOfDate {
        targets: failed_jobs(&output.stderr).map(str::to_owned).collect(),
        output: output.stderr,
    })
}

/// The modules a failed `lake setup-file` names: the module jobs it lists as
/// failed, then each import it calls bad (`collectImportsAux`,
/// `Lake/Build/Module.lean`), first mention first. A module job's name is the
/// module's, so the setup job, whose name holds a space and a path, drops out.
fn failed_modules(stderr: &str) -> Vec<String> {
    let bad_imports = stderr.lines().filter_map(|line| {
        line.split_once(": bad import '")
            .and_then(|(_, rest)| rest.split_once('\''))
            .map(|(module, _)| module)
    });
    let mut modules: Vec<String> = Vec::new();
    for name in failed_jobs(stderr).chain(bad_imports) {
        if !name.is_empty()
            && !name.contains(|c: char| c.is_whitespace() || c == ':' || c == '/')
            && !modules.iter().any(|known| known == name)
        {
            modules.push(name.to_owned());
        }
    }
    modules
}

/// Whether `lake query +MODULE:lean` failed because no module by that name
/// exists: no library holds the name (`unknownModule`, `Lake/CLI/Error.lean`),
/// or a library holds it but its source file is missing. Any other failure,
/// such as a header that does not parse, says nothing about the name.
fn names_no_module(module: &str, stderr: &str) -> bool {
    let source = format!("/{}.lean", module.replace('.', "/"));
    stderr.contains(&format!("unknown module `{module}`"))
        || (stderr.contains("no such file or directory")
            && stderr.lines().any(|line| {
                line.trim()
                    .strip_prefix("file: ")
                    .is_some_and(|path| path.ends_with(&source))
            }))
}

/// The error for an option lookup for `file_label` that failed with `error`.
/// Lake may have run the project's configuration, but no check of the file
/// started: the error says so, and the request records it.
fn options_failed(file_label: &str, error: ServerError) -> ServerError {
    let target_check = TargetCheckNotStarted {
        file_label: file_label.to_owned(),
        stage: "lake_option_discovery".to_owned(),
        disposition: "not_started".to_owned(),
    };
    CallCancellation::current().target_check_not_started(target_check.clone());
    let message = if let ServerError::BadProject(message) = error {
        message
    } else {
        error.to_string()
    };
    ServerError::PreparationFailed {
        message: format!("{message}; no proof check ran"),
        target_check,
    }
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
    options: BTreeMap<String, LeanOptionValue>,
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
            return Err(invalid_data("setup file no longer canonical"));
        }
        Ok(Self(m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec()))
    }
}

/// What `lake setup-file` printed for one file, with the identity of every
/// file it names, taken when Lake answered.
pub(crate) struct StagedSetup {
    pub(crate) bytes: Vec<u8>,
    pub(crate) options: BTreeMap<String, LeanOptionValue>,
    sysroot: PathBuf,
    /// The project's package roots ([`crate::lake_meta::manifest_package_roots`]).
    packages: Vec<PathBuf>,
    inputs: BTreeMap<PathBuf, Identity>,
}

impl StagedSetup {
    pub(super) fn artifact_identity_sha256(&self, shim_digest: &str) -> io::Result<String> {
        let executable = Identity::read(&self.sysroot.join("bin/lean"))?;
        let bytes = serde_json::to_vec(&(&self.sysroot, executable, &self.packages, &self.inputs, shim_digest))?;
        Ok(crate::digest::sha256_hex(&bytes))
    }

    /// Refuse the setup when the toolchain, the project's packages or any
    /// file it names changed since Lake answered, such as a build outside
    /// this server that replaced an import.
    pub(super) fn check(&self, root: &Path, sysroot: &Path) -> io::Result<()> {
        if self.sysroot != sysroot || self.packages != crate::lake_meta::manifest_package_roots(root) {
            return Err(invalid_data(
                "the project's packages or toolchain changed after Lake's setup",
            ));
        }
        for (p, identity) in &self.inputs {
            if Identity::read(p)? != *identity {
                return Err(invalid_data("a file Lake's setup names changed after Lake answered"));
            }
        }
        Ok(())
    }
}

/// The `count` paths `lake query --json` printed, one JSON string a line, as
/// canonical paths.
fn parse_sources(stdout: &[u8], count: usize) -> io::Result<Vec<PathBuf>> {
    let text = std::str::from_utf8(stdout).map_err(|_| invalid_data("Lake module sources are not UTF-8"))?;
    let sources = text
        .lines()
        .map(|line| {
            let path: PathBuf = serde_json::from_str(line)
                .map_err(|error| invalid_data(format!("invalid Lake module source: {error}")))?;
            path.canonicalize()
        })
        .collect::<io::Result<Vec<_>>>()?;
    if sources.len() != count {
        return Err(invalid_data(
            "Lake named a different number of module sources than asked",
        ));
    }
    Ok(sources)
}

fn parse_setup(bytes: &[u8]) -> io::Result<ModuleSetup> {
    if bytes.len() > crate::lean_run::PROGRAM_OUTPUT_LIMIT {
        return Err(invalid_data("Lake module setup exceeds capture limit"));
    }
    let setup: ModuleSetup =
        serde_json::from_slice(bytes).map_err(|error| invalid_data(format!("invalid Lake module setup: {error}")))?;
    if setup.name.is_empty() {
        return Err(invalid_data("missing Lake module name"));
    }
    Ok(setup)
}

pub(crate) fn stage(
    bytes: Vec<u8>,
    packages: &[PathBuf],
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
            || !(p.starts_with(&release_lib) || packages.iter().any(|r| p.starts_with(r.join(".lake/build"))))
        {
            return Err(invalid_data(
                "Lake setup artifact outside the project's package outputs or the selected release",
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
            return Err(invalid_data("unsupported Lake import artifact layout"));
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
                    return Err(invalid_data("empty plugin initializer"));
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
        packages: packages.to_vec(),
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
        // The read turn holds from setup-file until the check ends.
        self.ensure_read(&root).await?;
        let setup = self.setup_file(&root, Path::new(file), source).await?;
        let activity = self.preparation_activity();
        let reader = super::verify_file::PreparedReader::installed(meta)?;
        setup.check(&root, &reader.sysroot)?;
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

    /// The options Lake gives the Lean server for the file `label` names, as
    /// `lake setup-file` reports them under this request's read turn: the
    /// server options of the library that holds the file, or the root
    /// package's for a file outside every library. A label in angle brackets,
    /// such as `<elaborate>`, names no file and gets the root package's.
    ///
    /// Lake reads [`NO_IMPORTS`] in place of the file's header, so it builds
    /// and checks no import. It returns [`ServerError::OutOfDate`] only when
    /// the library's own needs, such as a plugin, must be built first. Any
    /// other failure says that the check of `label` never started.
    pub(super) async fn module_options(&self, root: &Path, label: &str) -> Result<BTreeMap<String, LeanOptionValue>> {
        self.ensure_read(root).await?;
        // Lake resolves a relative path against its working directory, the
        // root. A label in angle brackets names no file there, so Lake takes
        // it for a file outside every library.
        let file = if label.starts_with('<') {
            OsString::from(label)
        } else {
            root.join(label).into_os_string()
        };
        let args = vec!["setup-file".into(), file, NO_IMPORTS.into()];
        let lookup: Result<ModuleSetup> = async {
            let (_, output) = self
                .lake_reader(root, OPTION_DISCOVERY, args, &[0, NO_BUILD_EXIT])
                .await?;
            Ok(parse_setup(&setup_output(output)?)?)
        }
        .await;
        match lookup {
            Ok(setup) => {
                tracing::debug!(
                    root = %root.display(), module = %setup.name, lean_options = ?setup.options,
                    "Lake's server options; the host heartbeat limit overrides maxHeartbeats"
                );
                Ok(setup.options)
            }
            Err(error @ ServerError::OutOfDate { .. }) => Err(error),
            Err(error) => Err(options_failed(label, error)),
        }
    }

    /// The source file Lake reads for each of `modules`, in order, as
    /// `lake query +MODULE:lean` reports it under this request's read turn:
    /// the module's path under its library's `srcDir`. Lake builds nothing.
    pub(crate) async fn module_sources(&self, root: &Path, modules: &[&str]) -> Result<Vec<PathBuf>> {
        if modules.is_empty() {
            // `lake query` with no target queries the default targets.
            return Ok(Vec::new());
        }
        self.ensure_read(root).await?;
        let mut args: Vec<OsString> = vec!["query".into(), "--json".into()];
        args.extend(modules.iter().map(|module| format!("+{module}:lean").into()));
        let (_, output) = self.lake_reader(root, MODULE_SOURCES, args, &[0]).await?;
        Ok(parse_sources(&output.stdout, modules.len())?)
    }

    /// Which of `modules` name no module, as `lake query +MODULE:lean` tells
    /// under this request's read turn. A module Lake knows but has not built
    /// is not one of them. Lake builds nothing. Only the first
    /// [`LOOKUP_LIMIT`] names are looked up.
    async fn unknown_modules(&self, root: &Path, modules: &[String]) -> Result<Vec<String>> {
        let modules = modules.get(..LOOKUP_LIMIT).unwrap_or(modules);
        if modules.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_read(root).await?;
        let query = |modules: &[String]| {
            let mut args: Vec<OsString> = vec!["query".into()];
            args.extend(modules.iter().map(|module| format!("+{module}:lean").into()));
            args
        };
        // One name Lake cannot find fails the whole query, so a batch that
        // passes clears every name at once.
        if modules.len() > 1 {
            let (_, output) = self
                .lake_reader(root, MODULE_LOOKUP, query(modules), &[0, FAILURE_EXIT])
                .await?;
            if output.exit_code == 0 {
                return Ok(Vec::new());
            }
        }
        let mut unknown = Vec::new();
        for module in modules {
            let (_, output) = self
                .lake_reader(
                    root,
                    MODULE_LOOKUP,
                    query(std::slice::from_ref(module)),
                    &[0, FAILURE_EXIT],
                )
                .await?;
            if output.exit_code == FAILURE_EXIT && names_no_module(module, &output.stderr) {
                unknown.push(module.clone());
            }
        }
        Ok(unknown)
    }

    /// `err`, or [`ServerError::UnknownImports`] when the module whose
    /// `.olean` a check with session imports `imports` could not load names
    /// no module Lake knows. Lookup failures keep `err`.
    pub(super) async fn name_unknown_imports(&self, root: &Path, imports: &[String], err: ServerError) -> ServerError {
        if matches!(err, ServerError::OutOfDate { .. } | ServerError::UnknownImports { .. })
            || !crate::diagnosis::missing_olean_failure(&err)
        {
            return err;
        }
        let text = err.to_string();
        let candidates = crate::diagnosis::unloaded_modules(&text, imports);
        match self.unknown_modules(root, &candidates).await {
            Ok(modules) if !modules.is_empty() => ServerError::UnknownImports { modules, output: text },
            Ok(_) | Err(_) => err,
        }
    }

    /// The error for a `lake setup-file` that failed:
    /// [`ServerError::UnknownImports`] when the modules it names include one
    /// Lake finds no module for, else what Lake printed.
    async fn setup_failure(&self, root: &Path, output: super::reader::ReaderOutput) -> ServerError {
        match self.unknown_modules(root, &failed_modules(&output.stderr)).await {
            Ok(modules) if !modules.is_empty() => ServerError::UnknownImports {
                modules,
                output: output.stderr,
            },
            Ok(_) | Err(_) => output.exit_error(SETUP_FILE),
        }
    }

    /// Run `lake setup-file` as a short run. When an import is out of date,
    /// Lake builds nothing and this returns [`ServerError::OutOfDate`]; when
    /// an import names no module, [`ServerError::UnknownImports`].
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
        let packages = crate::lake_meta::manifest_package_roots(root);
        let args = vec!["setup-file".into(), file.as_os_str().to_owned()];
        let (sysroot, output) = self
            .lake_reader(root, SETUP_FILE, args, &[0, FAILURE_EXIT, NO_BUILD_EXIT])
            .await?;
        if output.exit_code == FAILURE_EXIT {
            return Err(self.setup_failure(root, output).await);
        }
        Ok(stage(setup_output(output)?, &packages, &sysroot, root, file)?)
    }

    /// Run the project's `lake --no-build --no-cache ARGS` in `root` as a
    /// short run, with Lake's caches pointed at the run's private inputs, and
    /// return the toolchain prefix with Lake's output. The run holds a share
    /// of the request's read turn, so no build starts while Lake reads.
    async fn lake_reader(
        &self,
        root: &Path,
        label: &'static str,
        args: Vec<OsString>,
        accepted: &'static [i32],
    ) -> Result<(PathBuf, super::reader::ReaderOutput)> {
        let id = crate::toolchain::ToolchainId::from_lake_root(root).map_err(invalid)?;
        if id.window_verdict() != crate::toolchain::WindowVerdict::Supported {
            return Err(ServerError::BadProject(format!(
                "{label} needs a supported toolchain, not {id}"
            )));
        }
        let sysroot = id.elan_dir().map_err(invalid)?.canonicalize()?;
        let activity = self.preparation_activity();
        let lake = sysroot.join("bin/lake");
        let cwd = root.to_owned();
        let output = super::reader::reader_run(
            label,
            root,
            crate::cancellation::CallCancellation::current()
                .remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.lean_runs(),
            Some(Box::new(activity)),
            accepted,
            move |assets| {
                // Point any Lake cache at the run's private inputs.
                let cache = assets.join("lake-cache");
                fs::create_dir(&cache)?;
                let mut command = vec![lake.into_os_string(), "--no-build".into(), "--no-cache".into()];
                command.extend(args);
                Ok(crate::lean_run::Program {
                    command,
                    environment: vec![
                        ("LAKE_CACHE_DIR".into(), cache.into_os_string()),
                        ("LAKE_ARTIFACT_CACHE".into(), "false".into()),
                    ],
                    cwd,
                    max_stdout_bytes: crate::lean_run::PROGRAM_OUTPUT_LIMIT,
                    stdin: None,
                })
            },
        )
        .await
        .map_err(|failure| failure.error)?;
        Ok((sysroot, output))
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
    fn sources_parse_one_json_path_a_line_and_must_all_be_there() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        fs::create_dir_all(root.join("src/Inner"))?;
        fs::write(root.join("src/Inner/X.lean"), "")?;
        fs::write(root.join("A.lean"), "")?;
        // Lake joins the package and library `srcDir`s, so `.` parts stay.
        let stdout = serde_json::to_string(&root.join("./src/Inner/X.lean"))?
            + "\n"
            + &serde_json::to_string(&root.join("././A.lean"))?
            + "\n";
        assert_eq!(
            parse_sources(stdout.as_bytes(), 2)?,
            [root.join("src/Inner/X.lean"), root.join("A.lean")]
        );
        assert!(parse_sources(stdout.as_bytes(), 3).is_err());
        assert!(parse_sources(b"null\n", 1).is_err());
        Ok(())
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
        let output = |exit_code| crate::broker::reader::ReaderOutput {
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
    fn a_failed_setup_names_its_module_jobs_and_bad_imports_once_each() {
        // What Lake 4.36 prints for `lake --no-build setup-file T.lean` when
        // T imports `Chain.Typo`, which has no source file, and `Chain.D`.
        let stderr = "✖ [2/4] Running Chain.Typo\n\
                      error: no such file or directory (error code: 2)\n  \
                      file: /p/Chain/Typo.lean\n\
                      ✖ [4/4] Running setup-file /p/T.lean\n\
                      error: /p/T.lean: bad import 'Chain.Typo'\n\
                      error: /p/T.lean: bad import 'Chain.D'\n\
                      Some required targets logged failures:\n\
                      - Chain.Typo\n\
                      - setup-file /p/T.lean\n\
                      Failed to build module dependencies.\n";
        assert_eq!(failed_modules(stderr), ["Chain.Typo", "Chain.D"]);
        assert!(failed_modules("error: something else\n").is_empty());
    }

    #[test]
    fn only_a_missing_name_or_source_names_no_module() {
        let missing_source = "error: no such file or directory (error code: 2)\n  file: /p/./Chain/Typo.lean\n";
        assert!(names_no_module("Chain.Typo", missing_source));
        // The missing file must be this module's source, not some other file.
        assert!(!names_no_module("Chain.Other", missing_source));
        assert!(names_no_module("Nope.Typo", "error: unknown module `Nope.Typo`\n"));
        assert!(!names_no_module("Nope.Typo", "error: unknown module `Nope.Typos`\n"));
        assert!(!names_no_module(
            "Chain.B",
            "error: /p/Chain/B.lean:1:0: unexpected token; expected command\n"
        ));
    }

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn a_failed_option_lookup_says_the_check_never_started() -> TestResult {
        let request = tokio_util::sync::CancellationToken::new();
        let (error, evidence) = crate::cancellation::with_cancellation_evidence(&request, async {
            options_failed(
                "Demo.lean",
                ServerError::BadProject("Lake option discovery: the program exited with code 1: why".to_owned()),
            )
        })
        .await;
        let target = evidence.target_check.ok_or("no target check was recorded")?;
        assert_eq!(
            (
                target.file_label.as_str(),
                target.stage.as_str(),
                target.disposition.as_str()
            ),
            ("Demo.lean", "lake_option_discovery", "not_started")
        );
        let error = rmcp::ErrorData::from(error);
        assert_eq!(
            error.message,
            "lake project not usable: Lake option discovery: the program exited with code 1: why; no proof check ran"
        );
        let stage = error.data.as_ref().and_then(|data| data.pointer("/target_check/stage"));
        assert_eq!(stage, Some(&serde_json::json!("lake_option_discovery")));
        Ok(())
    }

    /// A Lake project in a new directory, with the fixture's toolchain, whose
    /// package and libraries set options in each way Lake knows. `Strict`
    /// imports `Loose`, which nothing has built.
    fn options_project() -> TestResult<(tempfile::TempDir, PathBuf)> {
        let fixture = std::env::var_os("LEAN_HOST_MCP_TEST_FIXTURE").ok_or("set LEAN_HOST_MCP_TEST_FIXTURE")?;
        let pin = fs::read_to_string(Path::new(&fixture).join("lean-toolchain"))?;
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let files = [
            ("lean-toolchain", pin.as_str()),
            (
                "lake-manifest.json",
                r#"{"version":"1.1.0","packagesDir":".lake/packages","packages":[]}"#,
            ),
            (
                "lakefile.lean",
                "import Lake\nopen Lake DSL\n\npackage options_test where\n  \
                 leanOptions := #[⟨`pp.unicode.fun, true⟩]\n  \
                 moreServerOptions := #[⟨`linter.unusedVariables, false⟩]\n  \
                 moreLeanArgs := #[\"-DmaxRecDepth=321\"]\n\n\
                 lean_lib Strict where\n  leanOptions := #[⟨`autoImplicit, false⟩]\n  \
                 moreServerOptions := #[⟨`maxSynthPendingDepth, .ofNat 3⟩]\n\n\
                 lean_lib Loose where\n  srcDir := \"src\"\n",
            ),
            ("Strict.lean", "import Loose\n"),
            ("src/Loose.lean", "def loose : Nat := 1\n"),
            ("Outside.lean", "def outside : Nat := 1\n"),
        ];
        for (path, text) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, text)?;
        }
        Ok((dir, root))
    }

    /// The server takes each file's options from Lake: a library file gets
    /// its library's server options, which add `moreServerOptions` and leave
    /// out `-D` arguments; a file outside every library, and a label that
    /// names no file, get the root package's. A stale import does not fail
    /// the lookup, and Lake builds nothing.
    #[tokio::test]
    #[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE"]
    async fn options_are_the_server_options_lake_gives_each_file() -> TestResult {
        let (_dir, root) = options_project()?;
        let broker = ProjectBroker::new(super::super::BrokerConfig {
            lake_root: Some(root.clone()),
            ..super::super::BrokerConfig::unserved()
        });
        let package = BTreeMap::from([
            ("linter.unusedVariables".to_owned(), LeanOptionValue::Bool(false)),
            ("pp.unicode.fun".to_owned(), LeanOptionValue::Bool(true)),
        ]);
        let mut strict = package.clone();
        strict.insert("autoImplicit".to_owned(), LeanOptionValue::Bool(false));
        strict.insert("maxSynthPendingDepth".to_owned(), LeanOptionValue::Nat(3));
        // The first lookup also elaborates the configuration; the second
        // reads Lake's saved copy.
        let started = std::time::Instant::now();
        assert_eq!(broker.module_options(&root, "Strict.lean").await?, strict);
        let first = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(broker.module_options(&root, "Strict.lean").await?, strict);
        eprintln!("Lake's options took {first:?}, then {:?}", started.elapsed());
        let src = root.join("src/Loose.lean");
        for label in [src.to_str().ok_or("path is not UTF-8")?, "Outside.lean", "<elaborate>"] {
            assert_eq!(broker.module_options(&root, label).await?, package, "{label}");
        }
        assert!(!root.join(".lake/build").exists(), "Lake built something");
        Ok(())
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
            packages: crate::lake_meta::manifest_package_roots(&root),
            inputs: BTreeMap::from([(leaf.clone(), Identity::read(&leaf)?)]),
        };
        setup.check(&root, &setup.sysroot)?;
        let replacement = output.join("replacement");
        fs::write(&replacement, b"same bytes")?;
        fs::rename(replacement, leaf)?;
        assert!(matches!(setup.check(&root, &setup.sysroot), Err(error) if error.kind() == io::ErrorKind::InvalidData));
        Ok(())
    }
}
