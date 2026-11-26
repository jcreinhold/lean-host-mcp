//! `lean_build` and `lean_imports`: the tools that run Lake and read what it
//! built.

use std::path::{Path, PathBuf};

use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::ToolContext;
use super::source_input::{read_query_file, source_path_for_module};
use crate::broker::ProjectHint;
use crate::build::config::BuildConfig;
use crate::build::{BuildOutcome, BuildRequest, log_path, new_id, plan, run_build};
use crate::error::ServerError;

/// The most files one `lean_imports` call reads.
pub const MAX_IMPORT_FILES: usize = 16;

/// Run the build `request` asks for. A failing step is a reply with `ok`
/// false, not an error.
///
/// # Errors
/// Returns invalid params when the request does not fit the project's
/// config, and the broker's error when the build gets no turn, its writer
/// does not open, or the request is cancelled.
pub async fn lean_build(ctx: &ToolContext, request: BuildRequest) -> Result<BuildOutcome, McpError> {
    let root = ctx
        .broker
        .native_root(&ProjectHint::from_request(request.project.clone()))?;
    let config = BuildConfig::load(&root).map_err(|errors| ServerError::BadProject(errors.join("; ")))?;
    let mut planned = plan(&config, &request).map_err(|problem| McpError::invalid_params(problem, None))?;
    planned.cap_tail(ctx.config.output.max_field_bytes);
    let id = new_id();
    let store = crate::store::store_dir(&root, None);
    crate::store::create_store_dir(&store, &root).map_err(ServerError::Io)?;
    let log = log_path(&store, &id).map_err(ServerError::Io)?;
    let helper = crate::shared_writer::contract().map_err(ServerError::Io)?.executable;
    let lock = ctx.broker.project_lock(&root);
    Ok(run_build(&lock, &root, planned, &id, &log, &helper).await?)
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
    /// The module's source file, when its `.olean` lies under the Lake root's
    /// `.lake/build` and the file is at the path its name gives. Null otherwise.
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
    /// `invalid_request` for a file the server cannot read, or
    /// `setup_file_failed` when Lake gave no setup for it, for example
    /// because an import is missing or stale.
    pub code: String,
    /// Names the file. For `setup_file_failed` it quotes Lake's own output.
    pub message: String,
}

/// Why `lean_imports` has no reply.
#[derive(Debug)]
pub enum ImportsError {
    /// One file has no import list. The tool result says why.
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

/// Report each file's whole import closure, from `lake setup-file`. Lake
/// builds nothing here, so a file with a missing or stale import fails the
/// call; the reply never holds a partial list.
///
/// # Errors
/// Returns [`ImportsError::File`] for the first file without imports, and
/// [`ImportsError::Call`] for a bad file list or when the call gets no turn.
pub async fn lean_imports(ctx: &ToolContext, request: ImportsRequest) -> Result<ImportsReply, ImportsError> {
    if request.files.is_empty() || request.files.len() > MAX_IMPORT_FILES {
        return Err(McpError::invalid_params(format!("`files` needs 1 to {MAX_IMPORT_FILES} files"), None).into());
    }
    let root = ctx
        .broker
        .native_root(&ProjectHint::from_request(request.project))
        .map_err(McpError::from)?;
    let build = root.join(".lake").join("build");
    let mut files = Vec::with_capacity(request.files.len());
    for file in request.files {
        let query = read_query_file(&root, Path::new(&file))
            .map_err(|error| ImportsError::file("invalid_request", &file, &error))?;
        let (module, imports) = match ctx.broker.module_imports(&root, &query.resolved, &query.source).await {
            Ok(found) => found,
            // A wait or a cancellation is the call's, not the file's.
            Err(error @ ServerError::WorkerUnavailable(_)) => return Err(McpError::from(error).into()),
            Err(error) => return Err(ImportsError::file("setup_file_failed", &file, &error)),
        };
        let imports = imports
            .into_iter()
            .map(|(module, olean)| {
                let source = olean
                    .starts_with(&build)
                    .then(|| source_path_for_module(&root, &module))
                    .filter(|source| source.is_file())
                    .map(|source: PathBuf| source.display().to_string());
                Import {
                    module,
                    olean: olean.display().to_string(),
                    source,
                }
            })
            .collect();
        files.push(FileImports { file, module, imports });
    }
    Ok(ImportsReply { files })
}
