//! `SessionHost` — single owner of all `lean-rs` state.
//!
//! `lean-rs` types (`LeanRuntime`, `LeanHost`, `LeanCapabilities`,
//! `LeanSession`, `SessionPool`) are `!Send` and carry a `'lean` lifetime
//! anchored to a process-global runtime. They cannot cross `tokio` task
//! boundaries, and they cannot live inside an `Arc<Mutex<…>>` accessed from
//! multiple async tasks.
//!
//! The pattern used here: one dedicated OS thread owns all Lean state.
//! Async tool handlers submit `Request` enum values over an
//! [`mpsc`](tokio::sync::mpsc) channel and await a `oneshot` reply. From the
//! caller's perspective the host looks like an async actor; the lifetime
//! tangle stays hidden inside the worker thread.
//!
//! All public methods on [`SessionHost`] return [`crate::error::Result`].
//! Errors flow only for infrastructure failures (worker thread gone, Lean
//! runtime init failed, Lake project unusable) — Lean-domain failures
//! (parse, elaboration, kernel rejection, meta timeout) are part of the
//! `Ok` payload.

// `let _ = reply.send(…)` / `let _ = init_reply.send(…)` are the canonical
// "ignore failure when the receiver dropped" pattern; the dropped value is
// a oneshot::Sender whose only destructor is "wake the receiver if any".
// `needless_pass_by_value` flags actor-style closure args and the
// `worker_main` ownership transfer; both intentionally take ownership.
#![allow(let_underscore_drop, clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;

use lean_rs::LeanRuntime;
use lean_rs_host::host::process::{ProcessFileOutcome, ProcessModuleOutcome};
use lean_rs_host::meta::{
    LeanMetaOptions, LeanMetaResponse, LeanMetaTransparency, infer_type as meta_infer_type,
    is_def_eq as meta_is_def_eq, pp_expr as meta_pp_expr, whnf as meta_whnf,
};
use lean_rs_host::{
    LeanCapabilities, LeanDeclarationFilter, LeanElabFailure, LeanElabOptions, LeanHost, LeanKernelOutcome,
    LeanSession, LeanSeverity, LeanSourceRange, ProofSummary,
};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Result, ServerError};

/// Source-range projection — public fields mirroring `LeanSourceRange`.
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
    /// `LeanSeverity` is `#[non_exhaustive]`; a future variant falls back
    /// to `Info` rather than blocking the response.
    pub fn from_lean(s: LeanSeverity) -> Self {
        match s {
            LeanSeverity::Error => Self::Error,
            LeanSeverity::Warning => Self::Warning,
            LeanSeverity::Info | _ => Self::Info,
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

/// Structured failure payload — the projection of `LeanElabFailure` we send
/// over JSON. Failure is part of a successful tool call; this is never an
/// MCP error.
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
    /// One of `Checked`, `Rejected`, `Unavailable`, `Unsupported`, or
    /// `Unknown` (for non-exhaustive future variants).
    pub status: String,
    pub summary: Option<KernelSummary>,
    pub failure: Option<ElabFailure>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct KernelSummary {
    pub declaration_name: String,
    pub kind: String,
    pub type_signature: String,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct MetaOutcome {
    /// One of `Ok`, `Failed`, `TimeoutOrHeartbeat`, `Unsupported`, or
    /// `Unknown` (for non-exhaustive future variants).
    pub status: String,
    pub rendered: Option<String>,
    pub definitionally_equal: Option<bool>,
    pub failure: Option<ElabFailure>,
    /// Set when the optional `meta_pp_expr` shim was missing and rendering
    /// fell back to `Expr.toString`. Internal — tool handlers translate
    /// this into an envelope warning; the field never serialises.
    #[serde(skip)]
    #[schemars(skip)]
    pub raw_fallback_used: bool,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct DeclarationRow {
    pub name: String,
    pub kind: String,
    /// Pretty-printed type signature via `Expr.toString` (cheap, no notation,
    /// deterministic). `None` only when the declaration has no recoverable
    /// type — typically internal artifacts or non-definitional entries.
    pub type_signature: Option<String>,
    pub source: Option<SourceRange>,
}

#[derive(Debug, Clone)]
pub struct SessionHost {
    tx: mpsc::Sender<Request>,
    lake_root: String,
    lean_toolchain: String,
}

impl SessionHost {
    /// Spawn the dedicated session thread, init Lean, open the Lake project,
    /// load capabilities. Blocks on the worker's readiness signal.
    ///
    /// # Errors
    ///
    /// Returns `ServerError::Lean` if the Lean runtime fails to initialise,
    /// or `ServerError::BadProject` if the Lake project at `lake_root` does
    /// not open or its capability dylib does not export the required
    /// `lean_rs_host_*` symbols.
    pub fn spawn(lake_root: PathBuf, package: String, library: String, default_imports: Vec<String>) -> Result<Self> {
        type InitMsg = std::result::Result<(String, mpsc::Sender<Request>), ServerError>;
        let (init_tx, init_rx) = std::sync::mpsc::channel::<InitMsg>();
        let lake_root_owned = lake_root.clone();
        thread::Builder::new()
            .name("lean-host-mcp/session".to_owned())
            .spawn(move || {
                worker_main(lake_root_owned, package, library, default_imports, init_tx);
            })
            .map_err(|e| ServerError::Internal(format!("spawn worker thread: {e}")))?;

        let (toolchain, tx) = init_rx
            .recv()
            .map_err(|_| ServerError::Internal("worker thread died during init".into()))??;

        Ok(Self {
            tx,
            lake_root: lake_root.to_string_lossy().into_owned(),
            lean_toolchain: toolchain,
        })
    }

    pub fn lake_root(&self) -> &str {
        &self.lake_root
    }

    pub fn lean_toolchain(&self) -> &str {
        &self.lean_toolchain
    }

    async fn submit<T: Send + 'static>(&self, build: impl FnOnce(oneshot::Sender<Result<T>>) -> Request) -> Result<T> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.tx
            .send(build(resp_tx))
            .await
            .map_err(|_| ServerError::SessionGone)?;
        resp_rx.await.map_err(|_| ServerError::SessionGone)?
    }

    /// # Errors
    ///
    /// Infrastructure failures only (worker thread gone, Lean call panicked);
    /// Lean-reported elaboration failures travel in the inner `Err` variant
    /// without becoming a [`ServerError`].
    pub async fn elaborate(
        &self,
        source: String,
        imports: Vec<String>,
    ) -> Result<std::result::Result<ElabSuccess, ElabFailure>> {
        self.submit(|reply| Request::Elaborate { source, imports, reply }).await
    }

    /// # Errors
    ///
    /// Infrastructure failures only; kernel rejections travel via
    /// [`KernelOutcome::status`].
    pub async fn kernel_check(&self, source: String, imports: Vec<String>) -> Result<KernelOutcome> {
        self.submit(|reply| Request::KernelCheck { source, imports, reply })
            .await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn infer_type(&self, source: String, imports: Vec<String>) -> Result<MetaOutcome> {
        self.submit(|reply| Request::InferType { source, imports, reply }).await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn whnf(&self, source: String, imports: Vec<String>) -> Result<MetaOutcome> {
        self.submit(|reply| Request::Whnf { source, imports, reply }).await
    }

    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn is_def_eq(
        &self,
        lhs: String,
        rhs: String,
        imports: Vec<String>,
        transparency: LeanMetaTransparency,
    ) -> Result<MetaOutcome> {
        self.submit(|reply| Request::IsDefEq {
            lhs,
            rhs,
            imports,
            transparency,
            reply,
        })
        .await
    }

    /// Look up a declaration by fully-qualified name. Returns `None` when
    /// Lean reports the declaration as `"missing"`. Used by `hover_by_name`.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn describe(&self, name: String, imports: Vec<String>) -> Result<Option<DeclarationRow>> {
        self.submit(|reply| Request::Describe { name, imports, reply }).await
    }

    /// List every declaration in the open environment as a Vec of fully-
    /// qualified strings. Filter controls private / generated / internal
    /// inclusion (use [`LeanDeclarationFilter::default()`] for the
    /// declaration-browser preset).
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn list_declarations_strings(
        &self,
        filter: LeanDeclarationFilter,
        imports: Vec<String>,
    ) -> Result<Vec<String>> {
        self.submit(|reply| Request::ListDeclarationStrings { filter, imports, reply })
            .await
    }

    /// Bulk-describe every declaration in `names`. Output order matches
    /// input order; entries Lean reports as `"missing"` are omitted.
    /// Uses upstream's bulk `declaration_kind` / `declaration_type` shims
    /// to keep the rebuild affordable on six-figure environments.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn describe_bulk(&self, names: Vec<String>, imports: Vec<String>) -> Result<Vec<DeclarationRow>> {
        self.submit(|reply| Request::DescribeBulk { names, imports, reply })
            .await
    }

    /// Run `IO.processCommands` over body-only `source` with info collection
    /// enabled and return the upstream [`ProcessFileOutcome`]. The source
    /// must **not** carry an `import` header — see [`Self::process_module`]
    /// for the header-aware sibling that drives the
    /// `goal_at_position` / `type_at_position` / `references_of_name` tools.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only.
    pub async fn process_file(&self, source: String, imports: Vec<String>) -> Result<ProcessFileOutcome> {
        self.submit(|reply| Request::ProcessFile { source, imports, reply })
            .await
    }

    /// Run a full Lean source file (header + body) through Lean's frontend
    /// pipeline with info collection enabled. The header is parsed via
    /// `Lean.Parser.parseHeader`; `IO.processCommands` resumes from the
    /// parser state the header produced, so info-tree positions come back
    /// in the original file's coordinate system.
    ///
    /// The returned [`ProcessModuleOutcome`] has four arms: `Ok`
    /// (header parsed, env satisfies imports, body processed),
    /// `MissingImports` (body still ran but some imports aren't in the
    /// session's open env — soft warning), `HeaderParseFailed`
    /// (header didn't parse, body never ran), and `Unsupported`
    /// (capability dylib lacks the shim).
    ///
    /// Routes to the **default** session — the env the file's parsed
    /// header is validated against. Per-call import selection is not
    /// meaningful here because the file's own header decides what the
    /// projection ran with.
    ///
    /// # Errors
    ///
    /// Infrastructure failures only; every Lean-domain outcome is a
    /// variant of [`ProcessModuleOutcome`].
    pub async fn process_module(&self, source: String) -> Result<ProcessModuleOutcome> {
        self.submit(|reply| Request::ProcessModule { source, reply }).await
    }
}

#[derive(Debug)]
enum Request {
    Elaborate {
        source: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<std::result::Result<ElabSuccess, ElabFailure>>>,
    },
    KernelCheck {
        source: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<KernelOutcome>>,
    },
    InferType {
        source: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<MetaOutcome>>,
    },
    Whnf {
        source: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<MetaOutcome>>,
    },
    IsDefEq {
        lhs: String,
        rhs: String,
        imports: Vec<String>,
        transparency: LeanMetaTransparency,
        reply: oneshot::Sender<Result<MetaOutcome>>,
    },
    Describe {
        name: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<Option<DeclarationRow>>>,
    },
    ListDeclarationStrings {
        filter: LeanDeclarationFilter,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    DescribeBulk {
        names: Vec<String>,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<Vec<DeclarationRow>>>,
    },
    ProcessFile {
        source: String,
        imports: Vec<String>,
        reply: oneshot::Sender<Result<ProcessFileOutcome>>,
    },
    ProcessModule {
        source: String,
        reply: oneshot::Sender<Result<ProcessModuleOutcome>>,
    },
}

fn worker_main(
    lake_root: PathBuf,
    package: String,
    library: String,
    default_imports: Vec<String>,
    init_reply: std::sync::mpsc::Sender<std::result::Result<(String, mpsc::Sender<Request>), ServerError>>,
) {
    let runtime = match LeanRuntime::init() {
        Ok(r) => r,
        Err(e) => {
            let _ = init_reply.send(Err(ServerError::Lean(format!("runtime init: {e}"))));
            return;
        }
    };

    let host = match LeanHost::from_lake_project(runtime, &lake_root) {
        Ok(h) => h,
        Err(e) => {
            let _ = init_reply.send(Err(ServerError::BadProject(format!(
                "open lake project at {}: {e}",
                lake_root.display()
            ))));
            return;
        }
    };

    let caps = match host.load_capabilities(&package, &library) {
        Ok(c) => c,
        Err(e) => {
            let _ = init_reply.send(Err(ServerError::BadProject(format!(
                "load capabilities {package}/{library}: {e} (does this Lake project depend on lean-rs-host shims?)"
            ))));
            return;
        }
    };

    let toolchain = lean_toolchain_label(&lake_root);
    let (tx, mut rx) = mpsc::channel::<Request>(64);
    if init_reply.send(Ok((toolchain, tx))).is_err() {
        return;
    }

    let mut state = WorkerState {
        caps: &caps,
        sessions: HashMap::new(),
        default_imports,
    };

    while let Some(req) = rx.blocking_recv() {
        state.handle(req);
    }
}

struct WorkerState<'lean, 'h> {
    caps: &'h LeanCapabilities<'lean, 'h>,
    sessions: HashMap<Vec<String>, LeanSession<'lean, 'h>>,
    default_imports: Vec<String>,
}

impl<'lean, 'h> WorkerState<'lean, 'h> {
    fn session_for(&mut self, imports: Vec<String>) -> Result<&mut LeanSession<'lean, 'h>> {
        let key = if imports.is_empty() {
            self.default_imports.clone()
        } else {
            imports
        };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.sessions.entry(key.clone()) {
            let module_refs: Vec<&str> = key.iter().map(String::as_str).collect();
            let session = self
                .caps
                .session(&module_refs, None, None)
                .map_err(|e| ServerError::Lean(format!("import {key:?}: {e}")))?;
            slot.insert(session);
        }
        self.sessions
            .get_mut(&key)
            .ok_or_else(|| ServerError::Internal("session vanished after insert".into()))
    }

    fn handle(&mut self, req: Request) {
        match req {
            Request::Elaborate { source, imports, reply } => {
                let _ = reply.send(self.do_elaborate(&source, imports));
            }
            Request::KernelCheck { source, imports, reply } => {
                let _ = reply.send(self.do_kernel_check(&source, imports));
            }
            Request::InferType { source, imports, reply } => {
                let _ = reply.send(self.do_infer_type(&source, imports));
            }
            Request::Whnf { source, imports, reply } => {
                let _ = reply.send(self.do_whnf(&source, imports));
            }
            Request::IsDefEq {
                lhs,
                rhs,
                imports,
                transparency,
                reply,
            } => {
                let _ = reply.send(self.do_is_def_eq(&lhs, &rhs, imports, transparency));
            }
            Request::Describe { name, imports, reply } => {
                let _ = reply.send(self.do_describe(&name, imports));
            }
            Request::ListDeclarationStrings { filter, imports, reply } => {
                let _ = reply.send(self.do_list_declaration_strings(filter, imports));
            }
            Request::DescribeBulk { names, imports, reply } => {
                let _ = reply.send(self.do_describe_bulk(names, imports));
            }
            Request::ProcessFile { source, imports, reply } => {
                let _ = reply.send(self.do_process_file(&source, imports));
            }
            Request::ProcessModule { source, reply } => {
                let _ = reply.send(self.do_process_module(&source));
            }
        }
    }

    fn do_elaborate(
        &mut self,
        source: &str,
        imports: Vec<String>,
    ) -> Result<std::result::Result<ElabSuccess, ElabFailure>> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        match session.elaborate(source, None, &opts, None) {
            Ok(Ok(_expr)) => Ok(Ok(ElabSuccess { ok: true })),
            Ok(Err(failure)) => Ok(Err(project_failure(&failure))),
            Err(e) => Err(ServerError::Lean(format!("elaborate: {e}"))),
        }
    }

    fn do_kernel_check(&mut self, source: &str, imports: Vec<String>) -> Result<KernelOutcome> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        let outcome = session
            .kernel_check(source, &opts, None, None)
            .map_err(|e| ServerError::Lean(format!("kernel_check: {e}")))?;
        Ok(project_kernel_outcome(session, outcome))
    }

    // Two meta hops per call: one to compute the expression (infer_type /
    // whnf), one to pretty-print it via pp_expr. The optional pp_expr shim
    // may be absent on older capability dylibs; fall back to expr_to_string_raw
    // and flag the response so the tool handler can emit a warning.
    fn do_infer_type(&mut self, source: &str, imports: Vec<String>) -> Result<MetaOutcome> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        let expr = match session.elaborate(source, None, &opts, None) {
            Ok(Ok(expr)) => expr,
            Ok(Err(failure)) => return Ok(failure_meta(failure)),
            Err(e) => return Err(ServerError::Lean(format!("elaborate for infer_type: {e}"))),
        };
        let meta_opts = LeanMetaOptions::new();
        let response = session
            .run_meta(&meta_infer_type(), expr, &meta_opts, None)
            .map_err(|e| ServerError::Lean(format!("run_meta infer_type: {e}")))?;
        render_meta_response(session, response, &meta_opts)
    }

    fn do_whnf(&mut self, source: &str, imports: Vec<String>) -> Result<MetaOutcome> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        let expr = match session.elaborate(source, None, &opts, None) {
            Ok(Ok(expr)) => expr,
            Ok(Err(failure)) => return Ok(failure_meta(failure)),
            Err(e) => return Err(ServerError::Lean(format!("elaborate for whnf: {e}"))),
        };
        let meta_opts = LeanMetaOptions::new();
        let response = session
            .run_meta(&meta_whnf(), expr, &meta_opts, None)
            .map_err(|e| ServerError::Lean(format!("run_meta whnf: {e}")))?;
        render_meta_response(session, response, &meta_opts)
    }

    fn do_is_def_eq(
        &mut self,
        lhs: &str,
        rhs: &str,
        imports: Vec<String>,
        transparency: LeanMetaTransparency,
    ) -> Result<MetaOutcome> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        let lhs_expr = match session.elaborate(lhs, None, &opts, None) {
            Ok(Ok(e)) => e,
            Ok(Err(failure)) => return Ok(failure_meta(failure)),
            Err(e) => return Err(ServerError::Lean(format!("elaborate lhs: {e}"))),
        };
        let rhs_expr = match session.elaborate(rhs, None, &opts, None) {
            Ok(Ok(e)) => e,
            Ok(Err(failure)) => return Ok(failure_meta(failure)),
            Err(e) => return Err(ServerError::Lean(format!("elaborate rhs: {e}"))),
        };
        let meta_opts = LeanMetaOptions::new();
        let response = session
            .run_meta(&meta_is_def_eq(), (lhs_expr, rhs_expr, transparency), &meta_opts, None)
            .map_err(|e| ServerError::Lean(format!("run_meta is_def_eq: {e}")))?;
        Ok(project_meta_bool(response))
    }

    fn do_describe(&mut self, name: &str, imports: Vec<String>) -> Result<Option<DeclarationRow>> {
        let session = self.session_for(imports)?;
        let kind = match session.declaration_kind(name, None) {
            Ok(k) if k == "missing" => return Ok(None),
            Ok(k) => k,
            Err(e) => return Err(ServerError::Lean(format!("declaration_kind {name}: {e}"))),
        };
        let type_signature = render_type_signature(session, name)?;
        let source = session
            .declaration_source_range(name, None)
            .ok()
            .flatten()
            .map(project_source_range);
        Ok(Some(DeclarationRow {
            name: name.to_owned(),
            kind,
            type_signature,
            source,
        }))
    }

    fn do_list_declaration_strings(
        &mut self,
        filter: LeanDeclarationFilter,
        imports: Vec<String>,
    ) -> Result<Vec<String>> {
        let session = self.session_for(imports)?;
        session
            .list_declarations_strings(&filter, None, None)
            .map_err(|e| ServerError::Lean(format!("list_declarations_strings: {e}")))
    }

    fn do_process_file(&mut self, source: &str, imports: Vec<String>) -> Result<ProcessFileOutcome> {
        let session = self.session_for(imports)?;
        let opts = LeanElabOptions::new();
        session
            .process_with_info_tree(source, &opts, None)
            .map_err(|e| ServerError::Lean(format!("process_with_info_tree: {e}")))
    }

    fn do_process_module(&mut self, source: &str) -> Result<ProcessModuleOutcome> {
        let session = self.session_for(Vec::new())?;
        let opts = LeanElabOptions::new();
        session
            .process_module_with_info_tree(source, &opts, None)
            .map_err(|e| ServerError::Lean(format!("process_module_with_info_tree: {e}")))
    }

    fn do_describe_bulk(&mut self, names: Vec<String>, imports: Vec<String>) -> Result<Vec<DeclarationRow>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let session = self.session_for(imports)?;
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let kinds = session
            .declaration_kind_bulk(&refs, None, None)
            .map_err(|e| ServerError::Lean(format!("declaration_kind_bulk: {e}")))?;
        let types = session
            .declaration_type_bulk(&refs, None, None)
            .map_err(|e| ServerError::Lean(format!("declaration_type_bulk: {e}")))?;

        let mut out = Vec::with_capacity(names.len());
        for (idx, name) in names.iter().enumerate() {
            let kind = kinds.get(idx).cloned().unwrap_or_else(|| "missing".to_owned());
            if kind == "missing" {
                continue;
            }
            let ty_expr = types.get(idx).and_then(Option::as_ref);
            let type_signature = match ty_expr {
                Some(expr) => Some(
                    session
                        .expr_to_string_raw(expr, None)
                        .map_err(|e| ServerError::Lean(format!("expr_to_string_raw {name}: {e}")))?,
                ),
                None => None,
            };
            let source = session
                .declaration_source_range(name, None)
                .ok()
                .flatten()
                .map(project_source_range);
            out.push(DeclarationRow {
                name: name.clone(),
                kind,
                type_signature,
                source,
            });
        }
        Ok(out)
    }
}

/// Render a declaration's type via `Expr.toString`. `Ok(None)` means the
/// declaration has no recoverable type expression (Lean returned `none`).
fn render_type_signature(session: &mut LeanSession<'_, '_>, name: &str) -> Result<Option<String>> {
    let ty = session
        .declaration_type(name, None)
        .map_err(|e| ServerError::Lean(format!("declaration_type {name}: {e}")))?;
    match ty {
        Some(expr) => {
            let rendered = session
                .expr_to_string_raw(&expr, None)
                .map_err(|e| ServerError::Lean(format!("expr_to_string_raw {name}: {e}")))?;
            Ok(Some(rendered))
        }
        None => Ok(None),
    }
}

fn lean_toolchain_label(lake_root: &std::path::Path) -> String {
    let path = lake_root.join("lean-toolchain");
    std::fs::read_to_string(&path)
        .ok()
        .map_or_else(|| "unknown".into(), |s| s.trim().to_owned())
}

pub(crate) fn project_failure(failure: &LeanElabFailure) -> ElabFailure {
    ElabFailure {
        diagnostics: failure
            .diagnostics()
            .iter()
            .map(|d| Diagnostic {
                severity: Severity::from_lean(d.severity()),
                message: d.message().to_owned(),
                position: d.position().map(|p| Position {
                    line: p.line(),
                    column: p.column(),
                    end_line: p.end_line(),
                    end_column: p.end_column(),
                }),
                file: meaningful_file_label(d.file_label()),
            })
            .collect(),
        truncated: failure.truncated(),
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

fn project_kernel_outcome<'lean>(
    session: &mut LeanSession<'lean, '_>,
    outcome: LeanKernelOutcome<'lean>,
) -> KernelOutcome {
    match outcome {
        LeanKernelOutcome::Checked(evidence) => {
            let summary = session.summarize_evidence(&evidence, None).ok().map(project_summary);
            KernelOutcome {
                status: "Checked".into(),
                summary,
                failure: None,
            }
        }
        LeanKernelOutcome::Rejected(failure) => KernelOutcome {
            status: "Rejected".into(),
            summary: None,
            failure: Some(project_failure(&failure)),
        },
        LeanKernelOutcome::Unavailable(failure) => KernelOutcome {
            status: "Unavailable".into(),
            summary: None,
            failure: Some(project_failure(&failure)),
        },
        LeanKernelOutcome::Unsupported(failure) => KernelOutcome {
            status: "Unsupported".into(),
            summary: None,
            failure: Some(project_failure(&failure)),
        },
        _ => KernelOutcome {
            status: "Unknown".into(),
            summary: None,
            failure: None,
        },
    }
}

fn project_summary(summary: ProofSummary) -> KernelSummary {
    KernelSummary {
        declaration_name: summary.declaration_name().to_owned(),
        kind: summary.kind().to_owned(),
        type_signature: summary.type_signature().to_owned(),
    }
}

fn project_source_range(range: LeanSourceRange) -> SourceRange {
    SourceRange {
        file: range.file,
        start_line: range.start_line,
        start_column: range.start_column,
        end_line: range.end_line,
        end_column: range.end_column,
    }
}

/// Pretty-print the `Ok` payload of an infer-type / whnf response. Falls
/// back to `Expr.toString` when the optional `meta_pp_expr` shim is missing
/// on the loaded capability dylib, setting `raw_fallback_used` so the tool
/// handler can attach a warning to the envelope.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "LeanMetaResponse is #[non_exhaustive] upstream; the wildcard forwards future variants to project_meta_unit, which handles them"
)]
fn render_meta_response<'lean>(
    session: &mut LeanSession<'lean, '_>,
    response: LeanMetaResponse<lean_rs::LeanExpr<'lean>>,
    meta_opts: &LeanMetaOptions,
) -> Result<MetaOutcome> {
    let expr = match response {
        LeanMetaResponse::Ok(expr) => expr,
        non_ok => return Ok(project_meta_unit(non_ok)),
    };
    let pp = session
        .run_meta(&meta_pp_expr(), expr.clone(), meta_opts, None)
        .map_err(|e| ServerError::Lean(format!("run_meta pp_expr: {e}")))?;
    match pp {
        LeanMetaResponse::Ok(rendered) => Ok(MetaOutcome {
            status: "Ok".into(),
            rendered: Some(rendered),
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        }),
        LeanMetaResponse::Unsupported(_) => {
            let raw = session
                .expr_to_string_raw(&expr, None)
                .map_err(|e| ServerError::Lean(format!("expr_to_string_raw: {e}")))?;
            Ok(MetaOutcome {
                status: "Ok".into(),
                rendered: Some(raw),
                definitionally_equal: None,
                failure: None,
                raw_fallback_used: true,
            })
        }
        non_ok => Ok(project_meta_unit(non_ok)),
    }
}

/// Project a `LeanMetaResponse<T>` whose `Ok` payload we don't expose.
/// After [`render_meta_response`] takes the `Ok(expr)` branch this only
/// fires for non-Ok variants, but stays generic so callers don't need to
/// duplicate the failure projection.
fn project_meta_unit<T>(response: LeanMetaResponse<T>) -> MetaOutcome {
    match response {
        LeanMetaResponse::Ok(_) => MetaOutcome {
            status: "Ok".into(),
            rendered: None,
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        },
        LeanMetaResponse::Failed(failure) => MetaOutcome {
            status: "Failed".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        LeanMetaResponse::TimeoutOrHeartbeat(failure) => MetaOutcome {
            status: "TimeoutOrHeartbeat".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        LeanMetaResponse::Unsupported(failure) => MetaOutcome {
            status: "Unsupported".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        _ => MetaOutcome {
            status: "Unknown".into(),
            rendered: None,
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        },
    }
}

fn project_meta_bool(response: LeanMetaResponse<bool>) -> MetaOutcome {
    match response {
        LeanMetaResponse::Ok(b) => MetaOutcome {
            status: "Ok".into(),
            rendered: None,
            definitionally_equal: Some(b),
            failure: None,
            raw_fallback_used: false,
        },
        LeanMetaResponse::Failed(failure) => MetaOutcome {
            status: "Failed".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        LeanMetaResponse::TimeoutOrHeartbeat(failure) => MetaOutcome {
            status: "TimeoutOrHeartbeat".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        LeanMetaResponse::Unsupported(failure) => MetaOutcome {
            status: "Unsupported".into(),
            rendered: None,
            definitionally_equal: None,
            failure: Some(project_failure(&failure)),
            raw_fallback_used: false,
        },
        _ => MetaOutcome {
            status: "Unknown".into(),
            rendered: None,
            definitionally_equal: None,
            failure: None,
            raw_fallback_used: false,
        },
    }
}

fn failure_meta(failure: LeanElabFailure) -> MetaOutcome {
    MetaOutcome {
        status: "Failed".into(),
        rendered: None,
        definitionally_equal: None,
        failure: Some(project_failure(&failure)),
        raw_fallback_used: false,
    }
}
