//! `lean_build` and `lean_imports`: the tools that run Lake and read what it
//! built.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolContext;
use super::source_input::read_query_file;
use crate::broker::ProjectHint;
use crate::broker::module_setup::{HeaderImport, NoAnswer};
use crate::build::config::BuildConfig;
use crate::build::{BuildOutcome, BuildPlan, BuildRequest, OnRunning, log_path, new_id, plan, run_build};
use crate::error::ServerError;

/// The most files one `lean_imports` call reads.
pub const MAX_IMPORT_FILES: usize = 16;

/// The most modules one `lean_imports` call reads.
pub const MAX_IMPORT_MODULES: usize = 128;

/// Run the build `request` asks for. A failing step is a reply with `ok`
/// false, not an error.
///
/// # Errors
/// Returns invalid params when the request does not fit the project's
/// config, and the broker's error when the build gets no turn, a worker does
/// not exit, or the request is cancelled.
pub async fn lean_build(ctx: &ToolContext, request: BuildRequest) -> Result<BuildOutcome, McpError> {
    lean_build_as(ctx, request, &new_id()).await
}

/// [`lean_build`] as build `id`, which names the build in its log and the
/// log's file.
///
/// # Errors
/// As [`lean_build`].
pub(crate) async fn lean_build_as(
    ctx: &ToolContext,
    request: BuildRequest,
    id: &str,
) -> Result<BuildOutcome, McpError> {
    let build = plan_build(ctx, &request)?;
    let store = crate::store::store_dir(&build.root, None).map_err(ServerError::BadProject)?;
    crate::store::create_store_dir(&store, &build.root).map_err(ServerError::Io)?;
    let log = log_path(&store, id).map_err(ServerError::Io)?;
    run_planned(ctx, build, id, &log, None).await
}

/// A build checked against its project's config.
pub(crate) struct PlannedBuild {
    /// The canonical Lake root.
    pub(crate) root: PathBuf,
    pub(crate) plan: BuildPlan,
}

/// Check the build `request` asks for against its project and config.
///
/// # Errors
/// Returns invalid params when the request does not fit the project's config,
/// and the broker's error when the project is not this server's.
pub(crate) fn plan_build(ctx: &ToolContext, request: &BuildRequest) -> Result<PlannedBuild, McpError> {
    let root = ctx
        .broker
        .resolve(&ProjectHint::from_request(request.project.clone()))?;
    let config = BuildConfig::load(&root).map_err(|errors| ServerError::BadProject(errors.join("; ")))?;
    let mut planned = plan(&config, request).map_err(|problem| McpError::invalid_params(problem, None))?;
    planned.cap_tail(ctx.config.output.max_field_bytes);
    Ok(PlannedBuild { root, plan: planned })
}

/// Run `build` as build `id`, writing its output to `log`. `on_running` runs
/// once the build holds the write turn and the project's workers have exited.
///
/// # Errors
/// As [`lean_build`], and when `on_running` fails.
pub(crate) async fn run_planned(
    ctx: &ToolContext,
    build: PlannedBuild,
    id: &str,
    log: &Path,
    on_running: Option<OnRunning>,
) -> Result<BuildOutcome, McpError> {
    Ok(run_build(&ctx.broker, &build.root, build.plan, id, log, on_running).await?)
}

/// `lean_imports` arguments. Name at least one file or module.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportsRequest {
    /// Lake root. Defaults to the server's project.
    #[serde(default)]
    pub project: Option<String>,
    /// Lean source files, absolute or relative to the Lake root. At most 16.
    #[serde(default)]
    pub files: Vec<String>,
    /// Module names, such as `Mathlib.Logic.Basic`. At most 128.
    #[serde(default)]
    pub modules: Vec<String>,
}

/// `lean_imports` reply.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ImportsReply {
    /// One entry per requested file, in request order.
    pub files: Vec<FileImports>,
    /// One entry per requested module, in request order.
    pub modules: Vec<ModuleImports>,
}

/// A module's source file and the imports its header lists.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ModuleImports {
    /// The module as the request named it.
    pub module: String,
    /// The source file Lake reads for the module, under its library's
    /// `srcDir`. Null when `error` is set.
    pub source: Option<String>,
    /// The imports the module's header lists, in order. Lean adds `Init` to
    /// every file that does not start with `prelude` and lists it twice, once
    /// as a `meta` import. Empty when `error` is set.
    pub imports: Vec<DirectImport>,
    /// Why Lake gave no answer for the module: `unknown_module` when no
    /// package in the workspace holds it, as for `Init` and the rest of the
    /// Lean toolchain, or its library has no source file for it;
    /// `module_header_failed` when Lake could not read its header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<FailureDetail>,
}

/// One import as a module's header lists it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DirectImport {
    pub module: String,
    /// `import all`: the import exposes the module's private data too.
    pub all: bool,
    /// `public import`: files that import this module see the import too.
    /// True for every import of a file outside the module system.
    pub public: bool,
    /// `meta import`: Lean also imports the compiled code of every
    /// definition the import reaches.
    pub meta: bool,
}

impl From<HeaderImport> for DirectImport {
    fn from(import: HeaderImport) -> Self {
        Self {
            module: import.module,
            all: import.import_all,
            public: import.is_exported,
            meta: import.is_meta,
        }
    }
}

/// The imports of one file.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FileImports {
    /// The file as the request named it.
    pub file: String,
    /// The module Lake names for the file.
    pub module: String,
    /// Every module the file imports, directly or not.
    pub imports: Vec<Import>,
}

/// One imported module.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Import {
    pub module: String,
    /// The `.olean` Lake loads for it.
    pub olean: String,
    /// The module's source file, as Lake names it from its library's
    /// `srcDir`, when its `.olean` lies in the root package's import
    /// directory, the last one `lake env` puts on `LEAN_PATH`. Null for a
    /// dependency's module.
    pub source: Option<String>,
}

/// A failure the tool reports as its result, with `isError` set, rather
/// than as a protocol error: `{error: {code, message}}`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ToolFailure {
    pub error: FailureDetail,
}

/// What failed.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FailureDetail {
    /// For a whole call: `invalid_request` for a file the server cannot
    /// read, `unknown_import` when a file's import names no module Lake
    /// knows, `setup_file_failed` when Lake gave no setup for a file for
    /// another reason, for example because an import is stale,
    /// `module_sources_failed` when Lake named no import directories or no
    /// source for a module the root built, or `module_inputs_failed` when
    /// Lake answered for none of
    /// the named modules. For one module: `unknown_module` or
    /// `module_header_failed`.
    pub code: String,
    /// Names the file or module, if one failed, and quotes Lake's own
    /// output.
    pub message: String,
}

/// Why `lean_imports` has no reply.
#[derive(Debug)]
pub enum ImportsError {
    /// A file has no import list, Lake named no source for a module the root
    /// built, or Lake answered for none of the named modules. The tool
    /// result says why.
    File(ToolFailure),
    /// The call failed as a whole, for example while it waited for a build.
    Call(McpError),
}

impl From<McpError> for ImportsError {
    fn from(error: McpError) -> Self {
        Self::Call(error)
    }
}

impl ImportsError {
    fn file(code: &str, file: &str, error: &ServerError) -> Self {
        Self::failure(code, format!("{file}: {error}"))
    }

    fn failure(code: &str, message: String) -> Self {
        Self::File(ToolFailure {
            error: FailureDetail {
                code: code.to_owned(),
                message,
            },
        })
    }
}

/// Whether `name` can stand in a Lake target spec as a module name: it holds
/// no whitespace and none of the characters Lake reads as spec syntax.
fn is_module_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(|c: char| c.is_whitespace() || matches!(c, ':' | '/' | '@' | '+'))
}

/// Report each file's whole import closure, with sources Lake names, and
/// each module's source and direct imports.
///
/// A file's closure comes from `lake setup-file`, and the source of each
/// module the root built from `lake query`. A module's source and header come
/// from `lake query`. Lake builds nothing here, so a file with a missing or
/// stale import fails the call; the reply never holds a partial list. A
/// module Lake cannot answer for gets its own error.
///
/// # Errors
/// Returns [`ImportsError::File`] for the first file without imports, when
/// Lake names no source, or when Lake answers for no module, and
/// [`ImportsError::Call`] for a bad request or when the call gets no turn.
pub async fn lean_imports(ctx: &ToolContext, request: ImportsRequest) -> Result<ImportsReply, ImportsError> {
    let invalid = |message: String| ImportsError::Call(McpError::invalid_params(message, None));
    if request.files.is_empty() && request.modules.is_empty() {
        return Err(invalid("name at least one of `files` and `modules`".to_owned()));
    }
    if request.files.len() > MAX_IMPORT_FILES {
        return Err(invalid(format!("`files` takes at most {MAX_IMPORT_FILES} files")));
    }
    if request.modules.len() > MAX_IMPORT_MODULES {
        return Err(invalid(format!("`modules` takes at most {MAX_IMPORT_MODULES} modules")));
    }
    if let Some(name) = request.modules.iter().find(|name| !is_module_name(name)) {
        return Err(invalid(format!("`{name}` is not a module name")));
    }
    let root = ctx
        .broker
        .resolve(&ProjectHint::from_request(request.project))
        .map_err(McpError::from)?;
    let files = file_imports(ctx, &root, request.files).await?;
    let modules = module_headers(ctx, &root, &request.modules).await?;
    Ok(ImportsReply { files, modules })
}

/// Each module's source and the imports its header lists, from one
/// `lake query` batch, plus one more run for each name no package holds.
async fn module_headers(
    ctx: &ToolContext,
    root: &Path,
    modules: &[String],
) -> Result<Vec<ModuleImports>, ImportsError> {
    let names: Vec<&str> = modules.iter().map(String::as_str).collect();
    let answers = match ctx.broker.module_inputs(root, &names).await {
        Ok(answers) => answers,
        Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
        Err(error) => return Err(ImportsError::failure("module_inputs_failed", error.to_string())),
    };
    Ok(modules
        .iter()
        .zip(answers)
        .map(|(module, answer)| match answer {
            Ok(inputs) => ModuleImports {
                module: module.clone(),
                source: Some(inputs.source.display().to_string()),
                imports: inputs.imports.into_iter().map(DirectImport::from).collect(),
                error: None,
            },
            Err(why) => ModuleImports {
                module: module.clone(),
                source: None,
                imports: Vec::new(),
                error: Some(FailureDetail {
                    code: match why {
                        NoAnswer::Unknown(_) => "unknown_module",
                        NoAnswer::Failed(_) => "module_header_failed",
                    }
                    .to_owned(),
                    message: format!("{module}: {}", why.output()),
                }),
            },
        })
        .collect())
}

/// Each file's whole import closure, with the sources of the modules the
/// root built.
async fn file_imports(ctx: &ToolContext, root: &Path, files: Vec<String>) -> Result<Vec<FileImports>, ImportsError> {
    let mut closures = Vec::with_capacity(files.len());
    for file in files {
        let query = read_query_file(root, Path::new(&file))
            .map_err(|error| ImportsError::file("invalid_request", &file, &error))?;
        match ctx.broker.module_imports(root, &query.resolved, &query.source).await {
            Ok((module, imports)) => closures.push((file, module, imports)),
            // A wait or a cancellation is the call's, not the file's.
            Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
            Err(error @ ServerError::UnknownImports { .. }) => {
                return Err(ImportsError::file("unknown_import", &file, &error));
            }
            Err(error) => return Err(ImportsError::file("setup_file_failed", &file, &error)),
        }
    }
    // The root built the modules whose .olean lies in its own import
    // directory. Each setup above asked Lake for the roots, so this runs no
    // Lake unless a lakefile changed since.
    let package_dir = if closures.is_empty() {
        None
    } else {
        match ctx.broker.import_roots(root).await {
            Ok(roots) => crate::broker::module_setup::package_import_dir(&roots).map(Path::to_owned),
            Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
            Err(error) => return Err(ImportsError::failure("module_sources_failed", error.to_string())),
        }
    };
    // One Lake query names the sources of every module the root built.
    let built: Vec<&str> = closures
        .iter()
        .flat_map(|(_, _, imports)| imports)
        .filter(|(_, olean)| package_dir.as_ref().is_some_and(|dir| olean.starts_with(dir)))
        .map(|(module, _)| module.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let answers = match ctx.broker.module_sources(root, &built).await {
        Ok(answers) => answers,
        Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
        Err(error) => return Err(ImportsError::failure("module_sources_failed", error.to_string())),
    };
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for (module, answer) in built.iter().zip(answers) {
        match answer {
            Ok(path) => {
                sources.insert((*module).to_owned(), path.display().to_string());
            }
            Err(why) => {
                return Err(ImportsError::failure(
                    "module_sources_failed",
                    format!("Lake named no source for {module}: {}", why.output()),
                ));
            }
        }
    }
    Ok(closures
        .into_iter()
        .map(|(file, module, imports)| FileImports {
            file,
            module,
            imports: imports
                .into_iter()
                .map(|(module, olean)| Import {
                    source: sources.get(&module).cloned(),
                    module,
                    olean: olean.display().to_string(),
                })
                .collect(),
        })
        .collect())
}
