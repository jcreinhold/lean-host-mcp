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
use crate::build::config::BuildConfig;
use crate::build::{BuildOutcome, BuildPlan, BuildRequest, OnRunning, log_path, new_id, plan, run_build};
use crate::error::ServerError;

/// The most files one `lean_imports` call reads.
pub const MAX_IMPORT_FILES: usize = 16;

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

/// `lean_imports` arguments.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportsRequest {
    /// Lake root. Defaults to the server's project.
    #[serde(default)]
    pub project: Option<String>,
    /// Lean source files, absolute or relative to the Lake root. At most 16.
    pub files: Vec<String>,
}

/// `lean_imports` reply.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ImportsReply {
    pub files: Vec<FileImports>,
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
    /// `srcDir`, when its `.olean` lies under the Lake root's `.lake/build`.
    /// Null for a dependency's module.
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
    /// `invalid_request` for a file the server cannot read, `unknown_import`
    /// when an import names no module Lake knows, `setup_file_failed` when
    /// Lake gave no setup for it for another reason, for example because an
    /// import is stale, or `module_sources_failed` when Lake named no source
    /// for a module the root built.
    pub code: String,
    /// Names the file, if one failed, and quotes Lake's own output.
    pub message: String,
}

/// Why `lean_imports` has no reply.
#[derive(Debug)]
pub enum ImportsError {
    /// A file has no import list, or Lake named no source for a module the
    /// root built. The tool result says why.
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
        Self::File(ToolFailure {
            error: FailureDetail {
                code: code.to_owned(),
                message: format!("{file}: {error}"),
            },
        })
    }
}

/// Report each file's whole import closure, with sources Lake names.
///
/// The closure comes from `lake setup-file`, and the source of each module the
/// root built from `lake query`. Lake builds nothing here, so a file with a
/// missing or stale import fails the call; the reply never holds a partial
/// list.
///
/// # Errors
/// Returns [`ImportsError::File`] for the first file without imports or when
/// Lake names no source, and [`ImportsError::Call`] for a bad file list or
/// when the call gets no turn.
pub async fn lean_imports(ctx: &ToolContext, request: ImportsRequest) -> Result<ImportsReply, ImportsError> {
    if request.files.is_empty() || request.files.len() > MAX_IMPORT_FILES {
        return Err(McpError::invalid_params(format!("`files` needs 1 to {MAX_IMPORT_FILES} files"), None).into());
    }
    let root = ctx
        .broker
        .resolve(&ProjectHint::from_request(request.project))
        .map_err(McpError::from)?;
    let build = root.join(".lake").join("build");
    let mut closures = Vec::with_capacity(request.files.len());
    for file in request.files {
        let query = read_query_file(&root, Path::new(&file))
            .map_err(|error| ImportsError::file("invalid_request", &file, &error))?;
        match ctx.broker.module_imports(&root, &query.resolved, &query.source).await {
            Ok((module, imports)) => closures.push((file, module, imports)),
            // A wait or a cancellation is the call's, not the file's.
            Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
            Err(error @ ServerError::UnknownImports { .. }) => {
                return Err(ImportsError::file("unknown_import", &file, &error));
            }
            Err(error) => return Err(ImportsError::file("setup_file_failed", &file, &error)),
        }
    }
    // One Lake query names the sources of every module the root built.
    let built: Vec<&str> = closures
        .iter()
        .flat_map(|(_, _, imports)| imports)
        .filter(|(_, olean)| olean.starts_with(&build))
        .map(|(module, _)| module.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sources: BTreeMap<String, String> = match ctx.broker.module_sources(&root, &built).await {
        Ok(paths) => built
            .iter()
            .zip(paths)
            .map(|(module, path)| ((*module).to_owned(), path.display().to_string()))
            .collect(),
        Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
        Err(error) => {
            return Err(ImportsError::File(ToolFailure {
                error: FailureDetail {
                    code: "module_sources_failed".to_owned(),
                    message: error.to_string(),
                },
            }));
        }
    };
    let files = closures
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
        .collect();
    Ok(ImportsReply { files })
}
