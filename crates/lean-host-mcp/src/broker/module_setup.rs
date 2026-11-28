//! File setup, module sources, module headers and import roots from the
//! selected Lake. Keep Lake's original frame, including import visibility,
//! plugin entry points and compiler options.
//!
//! Requirement: no Lake run here overlaps a build, and each runs under the
//! same read turn as the check it serves. Every run goes through `lake_run`,
//! which takes the request's read turn on the project first.
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
    sync::Arc,
    time::Duration,
};

const SETUP_FILE: &str = "Lake setup-file";
const IMPORT_ROOTS: &str = "Lake env";
const MODULE_SOURCES: &str = "Lake module sources";
const MODULE_INPUTS: &str = "Lake module inputs";
const MODULE_LOOKUP: &str = "Lake module lookup";
const FILE_MODULE: &str = "Lake file module";
const OPTION_DISCOVERY: &str = "Lake option discovery";

/// The module name Lake gives a file outside every library
/// (`setupExternalModule`, `Lake/Build/Module.lean`).
pub(super) const EXTERNAL_MODULE: &str = "_unknown";

/// The line `reportResult` (`Lake/Build/Run.lean`) prints before it lists
/// the jobs that failed, one `- NAME` line each.
const FAILURES_HEADER: &str = "Some required targets logged failures:";

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

/// The names of the jobs that failed, as Lake lists them under
/// [`FAILURES_HEADER`].
fn failed_jobs(stderr: &str) -> impl Iterator<Item = &str> {
    stderr
        .lines()
        .skip_while(|line| *line != FAILURES_HEADER)
        .skip(1)
        .map_while(|line| line.strip_prefix("- "))
}

/// Whether `line` opens a job's report, `{icon} [{done}/{total}] {verb}
/// {caption}` (`reportJob`, `Lake/Build/Run.lean`).
fn opens_job_report(line: &str) -> bool {
    let is_count = |count: &str| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit());
    line.split_once(" [").is_some_and(|(icon, rest)| {
        icon.chars().count() == 1
            && rest
                .split_once(']')
                .and_then(|(counts, _)| counts.split_once('/'))
                .is_some_and(|(done, total)| is_count(done) && is_count(total))
    })
}

/// What Lake logged for the job named `job`: the lines under its report, up
/// to the next report or the list of failed jobs.
fn job_log(stderr: &str, job: &str) -> Option<String> {
    let caption = format!(" {job}");
    let mut lines = stderr.lines();
    // A report may end with the job's time, as ` (12ms)`.
    lines.find(|line| {
        opens_job_report(line)
            && line
                .split_once(&caption)
                .is_some_and(|(_, rest)| rest.is_empty() || rest.starts_with(" ("))
    })?;
    let log: Vec<&str> = lines
        .take_while(|line| !opens_job_report(line) && *line != FAILURES_HEADER)
        .collect();
    Some(log.join("\n"))
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

/// Why Lake gave no answer for a module a query named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoAnswer {
    /// No package in the workspace holds a module by this name, or the
    /// library whose roots cover the name has no source file for it. Holds
    /// what Lake printed.
    Unknown(String),
    /// Lake knows the module but could not answer, for example because its
    /// header does not parse. Holds what Lake printed about the module.
    Failed(String),
}

impl NoAnswer {
    /// What Lake printed.
    pub(crate) fn output(&self) -> &str {
        match self {
            Self::Unknown(output) | Self::Failed(output) => output,
        }
    }
}

/// Lake's answer for one module, or why it gave none.
pub(crate) type Answer<T> = std::result::Result<T, NoAnswer>;

/// The modules of `pending` that a failed `lake query` of `facets` names,
/// each with why Lake gave no answer. Lake stops at the first name no
/// package holds (`parseTargetSpecs`, `Lake/CLI/Build.lean`), so that name
/// comes alone; otherwise each failed job names its module, and the job's
/// log says whether the module's source is missing.
fn missing_modules(pending: &[&str], facets: &[&str], stderr: &str) -> Vec<(String, NoAnswer)> {
    if let Some(module) = pending
        .iter()
        .find(|module| stderr.contains(&format!("unknown module `{module}`")))
    {
        return vec![((*module).to_owned(), NoAnswer::Unknown(stderr.trim().to_owned()))];
    }
    let mut settled: Vec<(String, NoAnswer)> = Vec::new();
    for job in failed_jobs(stderr) {
        let Some(module) = pending.iter().find(|module| {
            facets.iter().any(|facet| {
                let spec = format!("{module}:{facet}");
                job == spec || job.ends_with(&format!("/{spec}"))
            })
        }) else {
            continue;
        };
        if settled.iter().any(|(known, _)| known == *module) {
            continue;
        }
        let log = job_log(stderr, job).unwrap_or_else(|| stderr.trim().to_owned());
        let why = if names_no_module(module, &log) {
            NoAnswer::Unknown(log)
        } else {
            NoAnswer::Failed(log)
        };
        settled.push(((*module).to_owned(), why));
    }
    settled
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
    /// The module Lake names for the file: [`EXTERNAL_MODULE`] for a file
    /// outside every library.
    pub(crate) module: String,
    pub(crate) options: BTreeMap<String, LeanOptionValue>,
    sysroot: PathBuf,
    /// The project's import roots ([`ProjectBroker::import_roots`]) that
    /// Lake gave when it staged this setup.
    pub(crate) roots: Arc<[PathBuf]>,
    inputs: BTreeMap<PathBuf, Identity>,
}

impl StagedSetup {
    pub(super) fn artifact_identity_sha256(&self, shim_digest: &str) -> io::Result<String> {
        let executable = Identity::read(&self.sysroot.join("bin/lean"))?;
        let bytes = serde_json::to_vec(&(&self.sysroot, executable, &self.roots, &self.inputs, shim_digest))?;
        Ok(crate::digest::sha256_hex(&bytes))
    }

    /// Refuse the setup when the toolchain or any file it names changed
    /// since Lake answered, such as a build outside this server that
    /// replaced an import. The files include the manifest and the toolchain
    /// pin, so a change to the project's packages refuses it too.
    pub(super) fn check(&self, sysroot: &Path) -> io::Result<()> {
        if self.sysroot != sysroot {
            return Err(invalid_data("the project's toolchain changed after Lake's setup"));
        }
        for (p, identity) in &self.inputs {
            if Identity::read(p)? != *identity {
                return Err(invalid_data("a file Lake's setup names changed after Lake answered"));
            }
        }
        Ok(())
    }
}

/// Lake's import roots for one project root, kept while the files Lake
/// configures its workspace from keep their stamp.
pub(super) struct ImportRoots {
    root: PathBuf,
    stamp: crate::lake_meta::ProjectInputStamp,
    roots: Arc<[PathBuf]>,
}

/// The root package's import directory among the project's import `roots`
/// ([`ProjectBroker::import_roots`]): Lake lists the root package last.
/// `None` only for an empty list, which Lake never gives.
pub(crate) fn package_import_dir(roots: &[PathBuf]) -> Option<&Path> {
    roots.last().map(PathBuf::as_path)
}

/// The workspace at `root` whose Lake comes from `sysroot`.
fn workspace(root: &Path, sysroot: &Path) -> lean_toolchain::LakeWorkspace {
    lean_toolchain::LakeWorkspace::new(root).lean_sysroot(sysroot)
}

/// The `count` results `lake query --json` printed, one line of JSON each.
fn query_lines(stdout: &[u8], count: usize) -> io::Result<Vec<String>> {
    let text = std::str::from_utf8(stdout).map_err(|_| invalid_data("Lake query output is not UTF-8"))?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if lines.len() != count {
        return Err(invalid_data("Lake printed a different number of results than asked"));
    }
    Ok(lines)
}

/// The source path `lake query --json +MODULE:lean` printed, made canonical.
fn parse_source(line: &str) -> io::Result<PathBuf> {
    let path: PathBuf =
        serde_json::from_str(line).map_err(|error| invalid_data(format!("invalid Lake module source: {error}")))?;
    path.canonicalize()
}

/// One import a module's header lists (`Lean.Import`).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HeaderImport {
    pub(crate) module: String,
    /// `import all`: the import exposes the module's private data too.
    pub(crate) import_all: bool,
    /// `public import`: files that import this module see the import too.
    /// True for every import of a file outside the module system.
    pub(crate) is_exported: bool,
    /// `meta import`: Lean also imports the compiled code of every
    /// definition the import reaches.
    pub(crate) is_meta: bool,
}

/// A module's header as `lake query --json +MODULE:header` prints it
/// (`Lean.ModuleHeader`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    imports: Vec<HeaderImport>,
    #[serde(rename = "isModule")]
    _is_module: bool,
}

/// A module's source file and the imports its header lists, as Lake reads
/// them.
pub(crate) struct ModuleInputs {
    pub(crate) source: PathBuf,
    /// In header order, with the `Init` that every file without `prelude`
    /// imports: Lean lists it twice, once as a `meta` import.
    pub(crate) imports: Vec<HeaderImport>,
}

/// The source and header `lake query --json +MODULE:lean +MODULE:header`
/// printed for one module.
fn parse_inputs(lines: &[String]) -> io::Result<ModuleInputs> {
    let [source, header] = lines else {
        return Err(invalid_data("Lake printed the wrong number of module facets"));
    };
    let header: Header =
        serde_json::from_str(header).map_err(|error| invalid_data(format!("invalid Lake module header: {error}")))?;
    Ok(ModuleInputs {
        source: parse_source(source)?,
        imports: header.imports,
    })
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

/// Read what `lake setup-file` printed for `file` and take the identity of
/// every file it names.
///
/// Each named file must be a canonical file. Each import's `.olean` and IR
/// files must also lie under one of the project's import `roots` or the
/// release's `lib`, since Lake writes them to its packages' import
/// directories. Lake decides where a dynlib or plugin lives, so either may
/// lie anywhere; the lakefile already ran as code when Lake answered.
pub(crate) fn stage(
    bytes: Vec<u8>,
    roots: Arc<[PathBuf]>,
    sysroot: &Path,
    root: &Path,
    file: &Path,
) -> io::Result<StagedSetup> {
    let setup = parse_setup(&bytes)?;
    // These fields pass through byte-for-byte. Only Lake/Lean interprets them.
    let release_lib = sysroot.join("lib");
    let mut inputs = BTreeMap::new();
    let mut validate = |p: &Path, import: bool| -> io::Result<()> {
        if !p.is_absolute() || p.canonicalize()? != p || !p.is_file() {
            return Err(invalid_data("Lake setup names a path that is not a canonical file"));
        }
        if import && !(p.starts_with(&release_lib) || roots.iter().any(|r| p.starts_with(r))) {
            return Err(invalid_data(
                "Lake setup names an import artifact outside the project's import roots and the selected release",
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
            validate(path, true)?;
        }
    }
    for path in &setup.dynlibs {
        validate(path, false)?;
    }
    for plugin in &setup.plugins {
        match plugin {
            Plugin::Path(p) => validate(p, false)?,
            Plugin::Named { path, init_fn } => {
                if init_fn.as_ref().is_some_and(String::is_empty) {
                    return Err(invalid_data("empty plugin initializer"));
                }
                validate(path, false)?;
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
        module: setup.name,
        options: setup.options,
        inputs,
        roots,
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
        // Setup-file takes the request's read turn, which holds until the
        // check ends.
        let setup = self.setup_file(&root, Path::new(file), source).await?;
        let activity = self.preparation_activity();
        let reader = super::verify_file::PreparedReader::installed(meta)?;
        setup.check(&reader.sysroot)?;
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
    /// the library's own needs, such as a plugin, must be built first, and the
    /// turn's own error when the request gets no turn. Any other failure says
    /// that the check of `label` never started.
    pub(super) async fn module_options(&self, root: &Path, label: &str) -> Result<BTreeMap<String, LeanOptionValue>> {
        // Lake resolves a relative path against its working directory, the
        // root. A label in angle brackets names no file there, so Lake takes
        // it for a file outside every library.
        let file = if label.starts_with('<') {
            OsString::from(label)
        } else {
            root.join(label).into_os_string()
        };
        match self.setup_without_imports(root, OPTION_DISCOVERY, file).await {
            Ok(setup) => {
                tracing::debug!(
                    root = %root.display(), module = %setup.name, lean_options = ?setup.options,
                    "Lake's server options; the host heartbeat limit overrides maxHeartbeats"
                );
                Ok(setup.options)
            }
            Err(error @ (ServerError::OutOfDate { .. } | ServerError::WorkerUnavailable(_))) => Err(error),
            Err(error) => Err(options_failed(label, error)),
        }
    }

    /// The module Lake names for `file`, or `None` for a file outside every
    /// library, as `lake setup-file` reports it under this request's read
    /// turn. Lake reads [`NO_IMPORTS`] in place of the file's header, so it
    /// builds and checks no import; it returns [`ServerError::OutOfDate`]
    /// only when the library's own needs, such as a plugin, must be built
    /// first.
    pub(crate) async fn file_module(&self, root: &Path, file: &Path) -> Result<Option<String>> {
        let setup = self
            .setup_without_imports(root, FILE_MODULE, file.as_os_str().to_owned())
            .await?;
        Ok((setup.name != EXTERNAL_MODULE).then_some(setup.name))
    }

    /// What `lake setup-file FILE` prints with [`NO_IMPORTS`] in place of the
    /// file's header, under this request's read turn.
    async fn setup_without_imports(&self, root: &Path, label: &'static str, file: OsString) -> Result<ModuleSetup> {
        let args = vec!["setup-file".into(), file, NO_IMPORTS.into()];
        let (_, output) = self.lake_reader(root, label, args, &[0, NO_BUILD_EXIT]).await?;
        Ok(parse_setup(&setup_output(output)?)?)
    }

    /// The source file Lake reads for each of `modules`, in order, as
    /// `lake query +MODULE:lean` reports it under this request's read turn:
    /// the module's path under its library's `srcDir`, or why Lake named
    /// none. Lake builds nothing.
    pub(crate) async fn module_sources(&self, root: &Path, modules: &[&str]) -> Result<Vec<Answer<PathBuf>>> {
        let answers = self.query_modules(root, MODULE_SOURCES, modules, &["lean"]).await?;
        let mut sources = Vec::with_capacity(answers.len());
        for answer in answers {
            sources.push(match answer {
                Ok(lines) => {
                    let [source] = lines.as_slice() else {
                        return Err(invalid_data("Lake printed the wrong number of module facets").into());
                    };
                    Ok(parse_source(source)?)
                }
                Err(why) => Err(why),
            });
        }
        Ok(sources)
    }

    /// The source file of each of `modules` and the imports its header
    /// lists, in order, as `lake query +MODULE:lean +MODULE:header` reports
    /// them under this request's read turn, or why Lake gave none. Lake reads
    /// each header and builds nothing.
    pub(crate) async fn module_inputs(&self, root: &Path, modules: &[&str]) -> Result<Vec<Answer<ModuleInputs>>> {
        let answers = self
            .query_modules(root, MODULE_INPUTS, modules, &["lean", "header"])
            .await?;
        let mut inputs = Vec::with_capacity(answers.len());
        for answer in answers {
            inputs.push(match answer {
                Ok(lines) => Ok(parse_inputs(&lines)?),
                Err(why) => Err(why),
            });
        }
        Ok(inputs)
    }

    /// Which of `modules` name no module, as `lake query +MODULE:lean` tells
    /// under this request's read turn. A module Lake knows but has not built
    /// is not one of them. Lake builds nothing. Only the first
    /// [`LOOKUP_LIMIT`] names are looked up.
    async fn unknown_modules(&self, root: &Path, modules: &[String]) -> Result<Vec<String>> {
        let modules: Vec<&str> = modules.iter().take(LOOKUP_LIMIT).map(String::as_str).collect();
        let answers = self.query_modules(root, MODULE_LOOKUP, &modules, &["lean"]).await?;
        Ok(modules
            .iter()
            .zip(answers)
            .filter(|(_, answer)| matches!(answer, Err(NoAnswer::Unknown(_))))
            .map(|(module, _)| (*module).to_owned())
            .collect())
    }

    /// Lake's answers to `lake query --json +MODULE:FACET` for each of
    /// `facets` and each of `modules`, in order, under this request's read
    /// turn: one line of JSON per facet, or why Lake gave none. Lake builds
    /// nothing.
    ///
    /// One name Lake cannot answer fails the whole query, which then prints
    /// nothing, so the names a failed run settles leave the batch and the
    /// rest run again. Lake stops at the first name no package holds, so each
    /// such name costs a run of its own; one run settles every failed job.
    async fn query_modules(
        &self,
        root: &Path,
        label: &'static str,
        modules: &[&str],
        facets: &[&str],
    ) -> Result<Vec<Answer<Vec<String>>>> {
        let mut pending: Vec<&str> = Vec::new();
        for module in modules {
            if !pending.contains(module) {
                pending.push(module);
            }
        }
        let mut answers: BTreeMap<String, Answer<Vec<String>>> = BTreeMap::new();
        // `lake query` with no target queries the default targets.
        while !pending.is_empty() && !facets.is_empty() {
            let mut args: Vec<OsString> = vec!["query".into(), "--json".into()];
            for module in &pending {
                args.extend(facets.iter().map(|facet| OsString::from(format!("+{module}:{facet}"))));
            }
            let (_, output) = self.lake_reader(root, label, args, &[0, FAILURE_EXIT]).await?;
            if output.exit_code == 0 {
                let lines = query_lines(&output.stdout, pending.len().saturating_mul(facets.len()))?;
                for (module, values) in pending.iter().zip(lines.chunks(facets.len())) {
                    answers.insert((*module).to_owned(), Ok(values.to_vec()));
                }
                break;
            }
            let settled = missing_modules(&pending, facets, &output.stderr);
            if settled.is_empty() {
                return Err(output.exit_error(label));
            }
            answers.extend(settled.into_iter().map(|(module, why)| (module, Err(why))));
            pending.retain(|module| !answers.contains_key(*module));
        }
        Ok(modules
            .iter()
            .map(|module| answers.get(*module).cloned().unwrap_or_else(|| Ok(Vec::new())))
            .collect())
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
        let roots = self.import_roots(root).await?;
        let args = vec!["setup-file".into(), file.as_os_str().to_owned()];
        let (sysroot, output) = self
            .lake_reader(root, SETUP_FILE, args, &[0, FAILURE_EXIT, NO_BUILD_EXIT])
            .await?;
        if output.exit_code == FAILURE_EXIT {
            return Err(self.setup_failure(root, output).await);
        }
        Ok(stage(setup_output(output)?, roots, &sysroot, root, file)?)
    }

    /// The project's import roots: the directories Lake puts on `LEAN_PATH`
    /// for the packages of the workspace at `root`, dependencies first and
    /// the root package last, as `lake env` reports them under this request's
    /// read turn. Lake builds nothing. A root may not exist until Lake builds
    /// its package.
    ///
    /// Lake configures the workspace from the lakefiles, the toolchain pin
    /// and the manifest, so the answer is kept until one of them changes. A
    /// dependency's own lakefile is not among them.
    pub(crate) async fn import_roots(&self, root: &Path) -> Result<Arc<[PathBuf]>> {
        if let Some(roots) = self.known_import_roots(root) {
            return Ok(roots);
        }
        let stamp = crate::lake_meta::input_stamp(root);
        let (sysroot, output) = self
            .lake_run(root, IMPORT_ROOTS, &[0], |sysroot| {
                let command = workspace(root, sysroot).lean_path_command().map_err(invalid)?;
                Ok(crate::lean_run::Program::from_command(
                    &command,
                    root,
                    crate::lean_run::OUTPUT_LIMIT,
                ))
            })
            .await?;
        let roots: Arc<[PathBuf]> = workspace(root, &sysroot)
            .read_lean_path(&output.into_process_output())
            .map_err(invalid)?
            .into();
        self.inner.lock().import_roots = Some(ImportRoots {
            root: root.to_owned(),
            stamp,
            roots: Arc::clone(&roots),
        });
        Ok(roots)
    }

    /// The import roots Lake last gave for `root`, if none of the files
    /// [`Self::import_roots`] watches changed since. Runs no Lake.
    pub(crate) fn known_import_roots(&self, root: &Path) -> Option<Arc<[PathBuf]>> {
        let stamp = crate::lake_meta::input_stamp(root);
        self.inner
            .lock()
            .import_roots
            .as_ref()
            .filter(|known| known.root == root && known.stamp == stamp)
            .map(|known| Arc::clone(&known.roots))
    }

    /// Run the project's `lake --no-build --no-cache ARGS` in `root`; see
    /// [`Self::lake_run`].
    async fn lake_reader(
        &self,
        root: &Path,
        label: &'static str,
        args: Vec<OsString>,
        accepted: &'static [i32],
    ) -> Result<(PathBuf, super::reader::ReaderOutput)> {
        self.lake_run(root, label, accepted, |sysroot| {
            let mut command = vec![
                sysroot.join("bin/lake").into_os_string(),
                "--no-build".into(),
                "--no-cache".into(),
            ];
            command.extend(args);
            Ok(crate::lean_run::Program {
                command,
                environment: Vec::new(),
                cwd: root.to_owned(),
                max_stdout_bytes: crate::lean_run::PROGRAM_OUTPUT_LIMIT,
                stdin: None,
            })
        })
        .await
    }

    /// Run the Lake program that `program` makes from the project's
    /// toolchain prefix as a short run, with Lake's caches pointed at the
    /// run's private inputs, and return the prefix with Lake's output.
    ///
    /// Every Lake run that reads the project for a request starts here, so
    /// this is where it takes the request's read turn on `root`: Lake waits
    /// for a running or queued build, and the check that follows in the same
    /// request keeps the same turn. The run holds a share of the turn until
    /// its process group has exited.
    async fn lake_run(
        &self,
        root: &Path,
        label: &'static str,
        accepted: &'static [i32],
        program: impl FnOnce(&Path) -> Result<crate::lean_run::Program> + Send,
    ) -> Result<(PathBuf, super::reader::ReaderOutput)> {
        self.ensure_read(root).await?;
        let id = crate::toolchain::ToolchainId::from_lake_root(root).map_err(invalid)?;
        if id.window_verdict() != crate::toolchain::WindowVerdict::Supported {
            return Err(ServerError::BadProject(format!(
                "{label} needs a supported toolchain, not {id}"
            )));
        }
        let sysroot = id.elan_dir().map_err(invalid)?.canonicalize()?;
        let mut program = program(&sysroot)?;
        let activity = self.preparation_activity();
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
                program
                    .environment
                    .push(("LAKE_CACHE_DIR".into(), Some(cache.into_os_string())));
                program
                    .environment
                    .push(("LAKE_ARTIFACT_CACHE".into(), Some("false".into())));
                Ok(program)
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
    fn query_results_are_one_line_each_and_must_all_be_there() -> io::Result<()> {
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
        let lines = query_lines(stdout.as_bytes(), 2)?;
        let sources = lines
            .iter()
            .map(|line| parse_source(line))
            .collect::<io::Result<Vec<_>>>()?;
        assert_eq!(sources, [root.join("src/Inner/X.lean"), root.join("A.lean")]);
        assert!(query_lines(stdout.as_bytes(), 3).is_err());
        assert!(parse_source("null").is_err());
        Ok(())
    }

    #[test]
    fn a_header_parses_as_lake_prints_it() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().canonicalize()?.join("Strict.lean");
        fs::write(&source, "import Loose\n")?;
        // What Lake 4.36 prints for `+Strict:lean +Strict:header`.
        let header = r#"{"imports":[{"importAll":false,"isExported":true,"isMeta":false,"module":"Init"},{"importAll":false,"isExported":true,"isMeta":true,"module":"Init"},{"importAll":false,"isExported":true,"isMeta":false,"module":"Loose"}],"isModule":false}"#;
        let inputs = parse_inputs(&[serde_json::to_string(&source)?, header.to_owned()])?;
        assert_eq!(inputs.source, source);
        let imports: Vec<(&str, bool)> = inputs
            .imports
            .iter()
            .map(|import| (import.module.as_str(), import.is_meta))
            .collect();
        assert_eq!(imports, [("Init", false), ("Init", true), ("Loose", false)]);
        assert!(parse_inputs(&[serde_json::to_string(&source)?]).is_err());
        assert!(parse_inputs(&[serde_json::to_string(&source)?, r#"{"imports":[]}"#.to_owned()]).is_err());
        Ok(())
    }

    #[test]
    fn a_failed_query_settles_the_unknown_name_lake_stopped_at() {
        // What Lake 4.36 prints for `+Strict:lean +Init:lean +Loose:lean`.
        let stderr = "error: unknown module `Init`\n";
        assert_eq!(
            missing_modules(&["Strict", "Init", "Loose"], &["lean"], stderr),
            [(
                "Init".to_owned(),
                NoAnswer::Unknown("error: unknown module `Init`".to_owned())
            )]
        );
        assert!(missing_modules(&["Strict", "Ini"], &["lean"], stderr).is_empty());
    }

    #[test]
    fn a_failed_query_settles_each_failed_job_from_its_own_log() {
        // What Lake 4.36 prints for `+Strict.Missing:lean +Strict.Bad:header
        // +Loose:lean`, where `Strict.Missing` has no source file and
        // `Strict/Bad.lean` holds a bare `import`.
        let stderr = "✖ [1/3] Running options_test/Strict.Missing:lean\n\
                      error: no such file or directory (error code: 2)\n  \
                      file: /p/./Strict/Missing.lean\n\
                      ✖ [2/3] Running options_test/Strict.Bad:header\n\
                      error: /p/./Strict/Bad.lean:2:0: unexpected end of input\n\
                      Some required targets logged failures:\n\
                      - options_test/Strict.Missing:lean\n\
                      - options_test/Strict.Bad:header\n\
                      error: build failed\n";
        assert_eq!(
            job_log(stderr, "options_test/Strict.Bad:header").as_deref(),
            Some("error: /p/./Strict/Bad.lean:2:0: unexpected end of input")
        );
        assert_eq!(job_log(stderr, "options_test/Strict:header"), None);
        assert_eq!(
            missing_modules(&["Strict.Missing", "Strict.Bad", "Loose"], &["lean", "header"], stderr),
            [
                (
                    "Strict.Missing".to_owned(),
                    NoAnswer::Unknown(
                        "error: no such file or directory (error code: 2)\n  file: /p/./Strict/Missing.lean".to_owned()
                    )
                ),
                (
                    "Strict.Bad".to_owned(),
                    NoAnswer::Failed("error: /p/./Strict/Bad.lean:2:0: unexpected end of input".to_owned())
                ),
            ]
        );
        // A failure that names no pending module settles nothing.
        assert!(missing_modules(&["Loose"], &["lean", "header"], stderr).is_empty());
        assert!(missing_modules(&["Loose"], &["lean"], "error: could not load the workspace\n").is_empty());
    }

    #[test]
    fn job_reports_have_an_icon_and_counts() {
        assert!(opens_job_report("✖ [2/3] Running options_test/Strict.Missing:lean"));
        assert!(opens_job_report("⚠ [10/12] Built Demo.A (1.2s)"));
        assert!(!opens_job_report("error: [2/3] not a report"));
        assert!(!opens_job_report("✖ [2/] Running A"));
        assert!(!opens_job_report("- options_test/Strict.Missing:lean"));
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

    /// Run `lookup` as one request that waits at most `patience`, and keep
    /// only how it failed. The lookup comes boxed, since some are large.
    async fn as_request<T>(
        patience: Duration,
        lookup: std::pin::Pin<Box<impl std::future::Future<Output = Result<T>>>>,
    ) -> Result<()> {
        let request = tokio_util::sync::CancellationToken::new();
        crate::cancellation::with_cancellation(&request, crate::cancellation::with_deadline(patience, lookup))
            .await
            .map(|_| ())
    }

    /// Each Lake lookup the server runs, on `file` in `root`, as its own
    /// request that waits at most `patience`.
    async fn lake_lookups(
        broker: &ProjectBroker,
        root: &Path,
        file: &Path,
        patience: Duration,
    ) -> Vec<(&'static str, Result<()>)> {
        let version = vec![OsString::from("--version")];
        let modules = ["A".to_owned()];
        vec![
            (
                "lake",
                as_request(patience, Box::pin(broker.lake_reader(root, "Lake", version, &[0]))).await,
            ),
            (
                "options",
                as_request(patience, Box::pin(broker.module_options(root, "A.lean"))).await,
            ),
            (
                "file module",
                as_request(patience, Box::pin(broker.file_module(root, file))).await,
            ),
            (
                "imports",
                as_request(patience, Box::pin(broker.module_imports(root, file, ""))).await,
            ),
            (
                "sources",
                as_request(patience, Box::pin(broker.module_sources(root, &["A"]))).await,
            ),
            (
                "inputs",
                as_request(patience, Box::pin(broker.module_inputs(root, &["A"]))).await,
            ),
            (
                "lookup",
                as_request(patience, Box::pin(broker.unknown_modules(root, &modules))).await,
            ),
            (
                "import roots",
                as_request(patience, Box::pin(broker.import_roots(root))).await,
            ),
        ]
    }

    /// No Lake run overlaps a build. While a build holds the write turn, each
    /// lookup waits until its deadline and names the build. Once the build
    /// ends, each goes on to run Lake, which fails here since the root has no
    /// `lean-toolchain`.
    #[tokio::test]
    async fn every_lake_run_waits_for_a_build() -> TestResult {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let file = root.join("A.lean");
        fs::write(&file, "")?;
        let broker = ProjectBroker::new(super::super::BrokerConfig {
            lake_root: Some(root.clone()),
            ..super::super::BrokerConfig::unserved()
        });
        let deadline = tokio::time::Instant::now()
            .checked_add(Duration::from_secs(5))
            .ok_or("deadline out of range")?;
        let lock = broker.project_lock(&root);
        let build = crate::project_lock::ensure_write(&lock, "build-1", deadline, &CallCancellation::default())
            .await
            .map_err(|refused| format!("the build got no turn: {refused:?}"))?;
        for (lookup, result) in lake_lookups(&broker, &root, &file, Duration::from_millis(100)).await {
            let Err(ServerError::WorkerUnavailable(info)) = result else {
                return Err(format!("{lookup} did not wait for the build: {result:?}").into());
            };
            assert_eq!(
                (info.reason.as_str(), info.blocked_by.as_deref()),
                ("project_admission_timeout", Some("build-1")),
                "{lookup}"
            );
        }
        drop(build);
        for (lookup, result) in lake_lookups(&broker, &root, &file, Duration::from_secs(5)).await {
            let Err(error) = result else {
                return Err(format!("{lookup} passed in a root with no toolchain").into());
            };
            assert!(
                error.to_string().contains("lean-toolchain file not found"),
                "{lookup}: {error}"
            );
        }
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

    /// The import roots are what `lake env` reports: for a workspace with no
    /// dependency, the package's own import directory alone. The broker keeps
    /// the answer until a file Lake configures the workspace from changes,
    /// and Lake builds nothing.
    #[tokio::test]
    #[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE"]
    async fn import_roots_are_what_lake_env_reports() -> TestResult {
        let (_dir, root) = options_project()?;
        let broker = ProjectBroker::new(super::super::BrokerConfig {
            lake_root: Some(root.clone()),
            ..super::super::BrokerConfig::unserved()
        });
        assert!(broker.known_import_roots(&root).is_none());
        let started = std::time::Instant::now();
        let roots = broker.import_roots(&root).await?;
        eprintln!("lake env took {:?}", started.elapsed());
        assert_eq!(&*roots, [root.join(".lake/build/lib/lean")]);
        assert_eq!(broker.known_import_roots(&root).as_deref(), Some(&*roots));
        let lakefile = root.join("lakefile.lean");
        let mut text = fs::read_to_string(&lakefile)?;
        text.push('\n');
        fs::write(&lakefile, text)?;
        assert!(
            broker.known_import_roots(&root).is_none(),
            "a lakefile edit drops the answer"
        );
        assert!(!root.join(".lake/build").exists(), "Lake built something");
        Ok(())
    }

    /// Lake names each module's source under its library's `srcDir` and
    /// reads its header; a name no package holds, a name with no source file
    /// and a header that does not parse each get their own answer, and the
    /// other names still get Lake's. Lake builds nothing.
    #[tokio::test]
    #[ignore = "needs a Lean toolchain: set LEAN_HOST_MCP_TEST_FIXTURE"]
    async fn module_inputs_are_what_lake_reads() -> TestResult {
        let (_dir, root) = options_project()?;
        fs::create_dir_all(root.join("Strict"))?;
        fs::write(root.join("Strict/Bad.lean"), "import\n")?;
        let broker = ProjectBroker::new(super::super::BrokerConfig {
            lake_root: Some(root.clone()),
            ..super::super::BrokerConfig::unserved()
        });
        let names = ["Strict", "Init", "Loose", "Strict.Missing", "Strict.Bad", "Strict"];
        let started = std::time::Instant::now();
        let inputs = broker.module_inputs(&root, &names).await?;
        // Three runs: `Init` stops the first, two failed jobs the second.
        eprintln!("Lake's module inputs took {:?}", started.elapsed());
        let [strict, init, loose, missing, bad, again] = inputs.as_slice() else {
            return Err("one answer per name".into());
        };
        let imports = |answer: &Answer<ModuleInputs>| -> TestResult<(PathBuf, Vec<String>)> {
            let inputs = answer.as_ref().map_err(|why| format!("no answer: {why:?}"))?;
            Ok((
                inputs.source.clone(),
                inputs.imports.iter().map(|import| import.module.clone()).collect(),
            ))
        };
        assert_eq!(
            imports(strict)?,
            (
                root.join("Strict.lean"),
                vec!["Init".into(), "Init".into(), "Loose".into()]
            )
        );
        assert_eq!(imports(again)?, imports(strict)?);
        assert_eq!(
            imports(loose)?,
            (root.join("src/Loose.lean"), vec!["Init".into(), "Init".into()])
        );
        assert!(matches!(init, Err(NoAnswer::Unknown(output)) if output.contains("unknown module `Init`")));
        assert!(matches!(missing, Err(NoAnswer::Unknown(output)) if output.contains("Strict/Missing.lean")));
        assert!(matches!(bad, Err(NoAnswer::Failed(output)) if output.contains("unexpected end of input")));

        let sources = broker.module_sources(&root, &["Loose"]).await?;
        assert!(matches!(sources.as_slice(), [Ok(path)] if *path == root.join("src/Loose.lean")));
        assert_eq!(
            broker
                .unknown_modules(&root, &["Strict.Missing".into(), "Loose".into(), "Init".into()])
                .await?,
            ["Strict.Missing", "Init"]
        );
        let started = std::time::Instant::now();
        assert_eq!(
            broker
                .file_module(&root, &root.join("src/Loose.lean"))
                .await?
                .as_deref(),
            Some("Loose")
        );
        eprintln!("Lake's file module took {:?}", started.elapsed());
        assert_eq!(broker.file_module(&root, &root.join("Outside.lean")).await?, None);
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
            module: "Dep".to_owned(),
            options: BTreeMap::new(),
            sysroot: root.join("release"),
            roots: Arc::from([output.clone()]),
            inputs: BTreeMap::from([(leaf.clone(), Identity::read(&leaf)?)]),
        };
        setup.check(&setup.sysroot)?;
        let replacement = output.join("replacement");
        fs::write(&replacement, b"same bytes")?;
        fs::rename(replacement, leaf)?;
        assert!(matches!(setup.check(&setup.sysroot), Err(error) if error.kind() == io::ErrorKind::InvalidData));
        Ok(())
    }
}
