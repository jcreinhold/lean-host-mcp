//! `SessionHost` — closure-channel actor over a supervised
//! `lean-rs-worker` child.
//!
//! The Lean runtime lives in a child process the supervisor restarts when a
//! tactic wedges, a typeclass loop runs away, or the child OOMs. The parent
//! sees only `LeanWorkerCapability` (Send) and short-lived
//! `LeanWorkerSession<'_>` borrows; the `'lean` lifetime tangle of the
//! original in-process implementation is gone.
//!
//! One owner of the capability at a time is enforced by parking it on a
//! dedicated OS thread named `"lean-host-mcp/session"`. Each public method
//! ships a typed closure to the thread over a `tokio::mpsc` channel; the
//! closure opens a fresh session inside the thread's stack frame, calls the
//! worker, projects the worker's Serialize+Deserialize result type into the
//! MCP-stable wire shape, and replies via `oneshot`.
//!
//! All public methods on [`SessionHost`] return [`crate::error::Result`].
//! Errors flow only for infrastructure failures (worker thread gone, worker
//! child unreachable, Lake project unusable) — Lean-domain failures (parse,
//! elaboration, kernel rejection, meta timeout) are part of the `Ok`
//! payload.

#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use lean_rs_worker::{
    LeanWorkerCapability, LeanWorkerCapabilityBuilder, LeanWorkerChild, LeanWorkerDeclarationFilter,
    LeanWorkerDeclarationRow, LeanWorkerElabFailure, LeanWorkerElabOptions, LeanWorkerElabResult, LeanWorkerError,
    LeanWorkerKernelResult, LeanWorkerKernelStatus, LeanWorkerMetaResult, LeanWorkerMetaTransparency,
    LeanWorkerProcessFileOutcome, LeanWorkerProcessModuleOutcome, LeanWorkerRendered, LeanWorkerRendering,
    LeanWorkerSourceRange,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Result, ServerError};

/// Source-range projection — public fields mirroring `LeanWorkerSourceRange`.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SourceRange {
    pub file: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Position {
    pub line: u32,
    pub column: u32,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

/// Wire-stable severity classification. The three Lean severities map to
/// `snake_case` strings so the field is uniform with every other status
/// discriminant in the server.
#[derive(Debug, Clone, Copy, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    /// The worker layer emits severity as a snake-case string; unknown
    /// values map to `Info` rather than blocking the response.
    fn from_worker(s: &str) -> Self {
        match s {
            "error" => Self::Error,
            "warning" => Self::Warning,
            _ => Self::Info,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub position: Option<Position>,
    /// Real source path when Lean attached one. Omitted for the synthetic
    /// `<elaborate>` label that elaboration-buffer calls always produce —
    /// the caller already knows which file they asked about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// Structured failure payload — the projection of `LeanWorkerElabFailure` we
/// send over JSON. Failure is part of a successful tool call; this is never
/// an MCP error.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ElabFailure {
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct ElabSuccess {
    pub ok: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct KernelOutcome {
    /// One of `Checked`, `Rejected`, `Unavailable`, or `Unsupported`.
    pub status: String,
    pub summary: Option<KernelSummary>,
    pub failure: Option<ElabFailure>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct KernelSummary {
    pub declaration_name: String,
    pub kind: String,
    /// Pretty-printed declaration type via Lean's `PrettyPrinter`
    /// (notation-aware), as the worker's `LeanWorkerKernelSummary` reports
    /// it.
    pub type_signature: String,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct MetaOutcome {
    /// One of `Ok`, `Failed`, `TimeoutOrHeartbeat`, or `Unsupported`.
    pub status: String,
    pub rendered: Option<String>,
    pub definitionally_equal: Option<bool>,
    pub failure: Option<ElabFailure>,
    /// Set when the worker reported `LeanWorkerRendering::Raw` — the
    /// optional `meta_pp_expr` shim was missing or reported `Unsupported`,
    /// and the rendering fell back to `Expr.toString`. Internal — tool
    /// handlers translate this into an envelope warning; the field never
    /// serialises.
    #[serde(skip)]
    #[schemars(skip)]
    pub raw_fallback_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationRow {
    pub name: String,
    pub kind: String,
    /// Pretty-printed type signature as the worker's
    /// `LeanWorkerDeclarationRow` reports it. `None` only when the
    /// declaration has no recoverable type — typically internal artifacts.
    pub type_signature: Option<String>,
    pub source: Option<SourceRange>,
}

type Job = Box<dyn FnOnce(&mut LeanWorkerCapability) + Send + 'static>;

#[derive(Debug, Clone)]
pub struct SessionHost {
    tx: mpsc::Sender<Job>,
    lake_root: String,
    lean_toolchain: String,
    default_imports: Vec<String>,
}

impl SessionHost {
    /// Spawn the dedicated session thread, start the worker child, open the
    /// Lake project, and capture the toolchain label.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::BadProject` if the worker bootstrap check
    /// reports a blocking finding (worker child unresolvable, capability
    /// preflight failing, handshake failing, …); `ServerError::Lean` if the
    /// supervisor reports a runtime failure during the initial session
    /// open; or `ServerError::Internal` if the OS rejects the thread.
    pub fn spawn(lake_root: PathBuf, package: String, library: String, default_imports: Vec<String>) -> Result<Self> {
        type InitMsg = std::result::Result<(String, mpsc::Sender<Job>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let toolchain_label = lean_toolchain_label(&lake_root);
        let lake_root_owned = lake_root.clone();
        let default_imports_for_builder = default_imports.clone();
        thread::Builder::new()
            .name("lean-host-mcp/session".to_owned())
            .spawn(move || {
                worker_main(
                    lake_root_owned,
                    package,
                    library,
                    default_imports_for_builder,
                    toolchain_label,
                    init_tx,
                );
            })
            .map_err(|e| ServerError::Internal(format!("spawn worker thread: {e}")))?;

        let (toolchain, tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("worker thread died during init".into()))??;

        Ok(Self {
            tx,
            lake_root: lake_root.to_string_lossy().into_owned(),
            lean_toolchain: toolchain,
            default_imports,
        })
    }

    pub fn lake_root(&self) -> &str {
        &self.lake_root
    }

    pub fn lean_toolchain(&self) -> &str {
        &self.lean_toolchain
    }

    fn effective_imports(&self, imports: Vec<String>) -> Vec<String> {
        if imports.is_empty() {
            self.default_imports.clone()
        } else {
            imports
        }
    }

    async fn submit<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut LeanWorkerCapability) -> Result<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: Job = Box::new(move |cap| {
            let _ = reply_tx.send(f(cap));
        });
        self.tx.send(job).await.map_err(|_| ServerError::SessionGone)?;
        reply_rx.await.map_err(|_| ServerError::SessionGone)?
    }

    /// # Errors
    ///
    /// Infrastructure failures only; Lean-reported elaboration failures
    /// travel in the inner `Err` variant.
    pub async fn elaborate(
        &self,
        source: String,
        imports: Vec<String>,
    ) -> Result<std::result::Result<ElabSuccess, ElabFailure>> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .elaborate(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_elab_result(result))
        })
        .await
    }

    /// # Errors
    ///
    /// Infrastructure failures only; kernel rejections travel via
    /// [`KernelOutcome::status`].
    pub async fn kernel_check(&self, source: String, imports: Vec<String>) -> Result<KernelOutcome> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .kernel_check(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_kernel_result(result))
        })
        .await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn infer_type(&self, source: String, imports: Vec<String>) -> Result<MetaOutcome> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .infer_type(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_rendered(result))
        })
        .await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn whnf(&self, source: String, imports: Vec<String>) -> Result<MetaOutcome> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .whnf(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_rendered(result))
        })
        .await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn is_def_eq(
        &self,
        lhs: String,
        rhs: String,
        imports: Vec<String>,
        transparency: LeanWorkerMetaTransparency,
    ) -> Result<MetaOutcome> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let result = session
                .is_def_eq(&lhs, &rhs, transparency, &elab_opts(), None, None)
                .map_err(map_worker_err)?;
            Ok(project_meta_bool(result))
        })
        .await
    }

    /// Look up a declaration by fully-qualified name. Returns `None` when
    /// the name is not in the session's open environment.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn describe(&self, name: String, imports: Vec<String>) -> Result<Option<DeclarationRow>> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let row = session.describe(&name, None, None).map_err(map_worker_err)?;
            Ok(row.map(project_declaration_row))
        })
        .await
    }

    /// List every declaration in the open environment as a Vec of fully-
    /// qualified strings. Filter controls private / generated / internal
    /// inclusion (use [`LeanWorkerDeclarationFilter::default()`] for the
    /// declaration-browser preset).
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn list_declarations_strings(
        &self,
        filter: LeanWorkerDeclarationFilter,
        imports: Vec<String>,
    ) -> Result<Vec<String>> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            session
                .list_declarations_strings(&filter, None, None)
                .map_err(map_worker_err)
        })
        .await
    }

    /// Bulk-describe every declaration in `names`. Output order matches
    /// input order; entries the worker reports as `kind == "missing"` are
    /// omitted from the output.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn describe_bulk(&self, names: Vec<String>, imports: Vec<String>) -> Result<Vec<DeclarationRow>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let rows = session.describe_bulk(&refs, None, None).map_err(map_worker_err)?;
            Ok(rows
                .into_iter()
                .filter(|r| r.kind != "missing")
                .map(project_declaration_row)
                .collect())
        })
        .await
    }

    /// Run elaboration over body-only `source` with info collection
    /// enabled and return the worker's outcome. The source must **not**
    /// carry an `import` header — see [`Self::process_module`] for the
    /// header-aware sibling.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn process_file(&self, source: String, imports: Vec<String>) -> Result<LeanWorkerProcessFileOutcome> {
        let imports = self.effective_imports(imports);
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            session
                .process_file(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)
        })
        .await
    }

    /// Run a full Lean source file (header + body) through the worker's
    /// frontend pipeline with info collection enabled. The header is
    /// parsed by Lean; `process_module` resumes from the parsed state, so
    /// info-tree positions come back in the original file's coordinate
    /// system.
    ///
    /// The returned outcome has four arms: `Ok` (header parsed, env
    /// satisfies imports, body processed), `MissingImports` (body still
    /// ran but some imports aren't in the session's open env — soft
    /// warning), `HeaderParseFailed` (header didn't parse, body never
    /// ran), and `Unsupported` (capability dylib lacks the shim).
    ///
    /// Routes to the **default** session — the env the file's parsed
    /// header is validated against. Per-call import selection is not
    /// meaningful here because the file's own header decides what the
    /// projection ran with.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only; every Lean-domain outcome is a
    /// variant of [`LeanWorkerProcessModuleOutcome`].
    pub async fn process_module(&self, source: String) -> Result<LeanWorkerProcessModuleOutcome> {
        let imports = self.default_imports.clone();
        self.submit(move |cap| {
            let mut session = cap
                .open_session_with_imports(imports, None, None)
                .map_err(map_worker_err)?;
            session
                .process_module(&source, &elab_opts(), None, None)
                .map_err(map_worker_err)
        })
        .await
    }
}

fn worker_main(
    lake_root: PathBuf,
    package: String,
    library: String,
    default_imports: Vec<String>,
    toolchain_label: String,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<Job>), ServerError>>,
) {
    let builder = LeanWorkerCapabilityBuilder::new(&lake_root, &package, &library, default_imports.iter())
        .worker_child(LeanWorkerChild::sibling("lean-host-mcp-worker"))
        .startup_timeout(Duration::from_secs(30))
        .long_running_requests();

    let report = builder.check();
    if let Some(first) = report.first_error() {
        let _ = init_reply.send(Err(ServerError::BadProject(format!(
            "{}: {}",
            first.code(),
            first.message()
        ))));
        return;
    }

    let mut capability = match builder.open() {
        Ok(cap) => cap,
        Err(err) => {
            let _ = init_reply.send(Err(map_worker_err(err)));
            return;
        }
    };

    let runtime_toolchain = capability.runtime_metadata().lean_version.unwrap_or(toolchain_label);

    let (tx, mut rx) = mpsc::channel::<Job>(64);
    if init_reply.send(Ok((runtime_toolchain, tx))).is_err() {
        return;
    }

    while let Some(job) = rx.blocking_recv() {
        job(&mut capability);
    }
}

fn elab_opts() -> LeanWorkerElabOptions {
    LeanWorkerElabOptions::new()
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "LeanWorkerError is upstream-evolving; everything outside the bootstrap-classification set maps to Lean for the MCP wire"
)]
fn map_worker_err(err: LeanWorkerError) -> ServerError {
    match err {
        LeanWorkerError::WorkerChildUnresolved { .. }
        | LeanWorkerError::WorkerChildNotExecutable { .. }
        | LeanWorkerError::Bootstrap { .. }
        | LeanWorkerError::CapabilityBuild { .. }
        | LeanWorkerError::Setup { .. }
        | LeanWorkerError::Handshake { .. }
        | LeanWorkerError::CapabilityMetadataMismatch { .. } => ServerError::BadProject(err.to_string()),
        _ => ServerError::Lean(err.to_string()),
    }
}

fn lean_toolchain_label(lake_root: &std::path::Path) -> String {
    let path = lake_root.join("lean-toolchain");
    std::fs::read_to_string(&path)
        .ok()
        .map_or_else(|| "unknown".into(), |s| s.trim().to_owned())
}

// --- projection helpers -------------------------------------------------

pub(crate) fn project_diagnostic(d: &lean_rs_worker::LeanWorkerDiagnostic) -> Diagnostic {
    let position = d.line.map(|line| Position {
        line,
        column: d.column.unwrap_or(0),
        end_line: d.end_line,
        end_column: d.end_column,
    });
    Diagnostic {
        severity: Severity::from_worker(&d.severity),
        message: d.message.clone(),
        position,
        file: meaningful_file_label(&d.file_label),
    }
}

pub(crate) fn project_failure(failure: &LeanWorkerElabFailure) -> ElabFailure {
    ElabFailure {
        diagnostics: failure.diagnostics.iter().map(project_diagnostic).collect(),
        truncated: failure.truncated,
    }
}

fn project_source_range(range: LeanWorkerSourceRange) -> SourceRange {
    SourceRange {
        file: range.file,
        start_line: range.start_line,
        start_column: range.start_column,
        end_line: range.end_line,
        end_column: range.end_column,
    }
}

/// Strip Lean's synthetic source label so it never reaches the wire. Every
/// call that elaborates a string buffer (rather than a file on disk) gets
/// labelled `<elaborate>` — it is never actionable for the caller.
fn meaningful_file_label(label: &str) -> Option<String> {
    if label.is_empty() || label == "<elaborate>" || label.starts_with('<') {
        None
    } else {
        Some(label.to_owned())
    }
}

fn project_elab_result(result: LeanWorkerElabResult) -> std::result::Result<ElabSuccess, ElabFailure> {
    if result.success {
        Ok(ElabSuccess { ok: true })
    } else {
        Err(ElabFailure {
            diagnostics: result.diagnostics.iter().map(project_diagnostic).collect(),
            truncated: result.truncated,
        })
    }
}

fn project_kernel_result(result: LeanWorkerKernelResult) -> KernelOutcome {
    let status = match result.status {
        LeanWorkerKernelStatus::Checked => "Checked",
        LeanWorkerKernelStatus::Rejected => "Rejected",
        LeanWorkerKernelStatus::Unavailable => "Unavailable",
        LeanWorkerKernelStatus::Unsupported => "Unsupported",
    };
    let summary = result.summary.map(|s| KernelSummary {
        declaration_name: s.declaration_name,
        kind: s.kind,
        type_signature: s.type_signature,
    });
    let failure = if matches!(result.status, LeanWorkerKernelStatus::Checked) {
        None
    } else {
        Some(ElabFailure {
            diagnostics: result.diagnostics.iter().map(project_diagnostic).collect(),
            truncated: result.truncated,
        })
    };
    KernelOutcome {
        status: status.to_owned(),
        summary,
        failure,
    }
}

fn project_meta_rendered(result: LeanWorkerMetaResult<LeanWorkerRendered>) -> MetaOutcome {
    match result {
        LeanWorkerMetaResult::Ok { value } => MetaOutcome {
            status: "Ok".into(),
            rendered: Some(value.value),
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: matches!(value.rendering, LeanWorkerRendering::Raw),
        },
        LeanWorkerMetaResult::Failed { failure } => meta_failure("Failed", &failure),
        LeanWorkerMetaResult::TimeoutOrHeartbeat { failure } => meta_failure("TimeoutOrHeartbeat", &failure),
        LeanWorkerMetaResult::Unsupported { failure } => meta_failure("Unsupported", &failure),
    }
}

fn project_meta_bool(result: LeanWorkerMetaResult<bool>) -> MetaOutcome {
    match result {
        LeanWorkerMetaResult::Ok { value } => MetaOutcome {
            status: "Ok".into(),
            rendered: None,
            definitionally_equal: Some(value),
            failure: None,
            raw_fallback_used: false,
        },
        LeanWorkerMetaResult::Failed { failure } => meta_failure("Failed", &failure),
        LeanWorkerMetaResult::TimeoutOrHeartbeat { failure } => meta_failure("TimeoutOrHeartbeat", &failure),
        LeanWorkerMetaResult::Unsupported { failure } => meta_failure("Unsupported", &failure),
    }
}

fn meta_failure(status: &str, failure: &LeanWorkerElabFailure) -> MetaOutcome {
    MetaOutcome {
        status: status.to_owned(),
        rendered: None,
        definitionally_equal: None,
        failure: Some(project_failure(failure)),
        raw_fallback_used: false,
    }
}

fn project_declaration_row(row: LeanWorkerDeclarationRow) -> DeclarationRow {
    DeclarationRow {
        name: row.name,
        kind: row.kind,
        type_signature: row.type_signature,
        source: row.source.map(project_source_range),
    }
}

/// Re-export of [`LeanWorkerProcessedFile`] so callers that hold a cached
/// projection can pattern-match without importing `lean-rs-worker` directly.
pub use lean_rs_worker::LeanWorkerProcessedFile as ProcessedFile;
