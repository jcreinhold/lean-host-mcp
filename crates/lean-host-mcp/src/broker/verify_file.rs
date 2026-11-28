//! One-shot checks of current source against the project's build products.
//! Each check is a short run ([`crate::lean_run`]) of `lean` with the module
//! setup that `lake setup-file` prints for the file.
use super::{
    BrokerCall, Duration, LeanWorkerDeclarationVerificationBatchRequest, LeanWorkerDeclarationVerificationBatchResult,
    LeanWorkerElabOptions, Path, PathBuf, ProjectBroker, ProjectHint, Result, RuntimeFacts, reader_freshness,
};
use crate::error::invalid;
use serde::Deserialize;
use std::{fs, io};

fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    if path.canonicalize()? != path || !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("noncanonical prepared file"));
    }
    if crate::digest::file_sha256_hex(path)? != expected {
        return Err(invalid("prepared verification artifact changed"));
    }
    Ok(())
}

/// The installed verification shims a file check reads, and their identity.
pub(super) struct PreparedReader {
    pub(super) sysroot: PathBuf,
    pub(super) shims: Vec<PathBuf>,
    pub(super) roots: Vec<PathBuf>,
    shim_identity_sha256: String,
}

impl PreparedReader {
    /// The shims installed for `meta`'s toolchain, each checked against the
    /// hash the install recorded.
    pub(super) fn installed(meta: &crate::lake_meta::LakeProjectMeta) -> Result<Self> {
        let shims = VerifierShims::select(meta)?;
        shims.validate()?;
        let shim_identity_sha256 = crate::digest::sha256_hex(
            &serde_json::to_vec(&(&shims.sysroot, &shims.paths, &shims.files)).map_err(invalid)?,
        );
        Ok(Self {
            shim_identity_sha256,
            sysroot: shims.sysroot,
            shims: shims.paths,
            roots: Vec::new(),
        })
    }
}

pub(crate) struct VerifierShims {
    pub(crate) sysroot: PathBuf,
    pub(crate) paths: Vec<PathBuf>,
    files: std::collections::BTreeMap<PathBuf, String>,
}

impl VerifierShims {
    pub(crate) fn select(meta: &crate::lake_meta::LakeProjectMeta) -> Result<Self> {
        let (worker, sysroot, _) = crate::project::resolve_worker(meta)?;
        let generation = worker.parent().ok_or_else(|| invalid("missing worker generation"))?;
        let installed = crate::toolchain::WorkerSidecar::load(generation)
            .and_then(crate::toolchain::WorkerSidecar::into_runtime)
            .ok_or_else(|| invalid("missing installed shim deployment"))?;
        if installed.sysroot != sysroot {
            return Err(invalid("incomplete prepared import selection"));
        }
        Ok(Self {
            sysroot,
            paths: installed.prepared.shim_search_paths.iter().map(PathBuf::from).collect(),
            files: installed.files,
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for (path, hash) in &self.files {
            verify_hash(path, hash)?;
        }
        for module in crate::toolchain::VERIFIER_MODULES {
            let path = lean_toolchain::olean_candidate(&self.paths, module, |path| fs::metadata(path))
                .map_err(invalid)?
                .ok_or_else(|| invalid("missing verification driver shim module"))?;
            if !self.files.contains_key(&path) {
                return Err(invalid(
                    "installed generation lacks verification driver module identity",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupVerdict {
    runtime_lean_version: String,
    outcome: LeanWorkerDeclarationVerificationBatchResult,
}

fn selected_lean_version(toolchain: &str) -> &str {
    let label = toolchain.strip_prefix("leanprover/lean4:").unwrap_or(toolchain);
    label.strip_prefix('v').unwrap_or(label)
}

fn decode_setup_verdict(bytes: &[u8], toolchain: &str) -> Result<SetupVerdict> {
    let frame: SetupVerdict = serde_json::from_slice(bytes).map_err(invalid)?;
    if frame.runtime_lean_version != selected_lean_version(toolchain) {
        return Err(invalid("executing Lean version differs from the selected toolchain"));
    }
    Ok(frame)
}

impl ProjectBroker {
    pub(crate) async fn verify_file(
        &self,
        hint: ProjectHint,
        imports: Vec<String>,
        freshness: Vec<String>,
        request: LeanWorkerDeclarationVerificationBatchRequest,
        options: LeanWorkerElabOptions,
        detail: crate::tools::proof_action::LeanVerifyDetail,
    ) -> Result<BrokerCall<LeanWorkerDeclarationVerificationBatchResult>> {
        use lean_rs_worker_protocol::types::{
            LeanWorkerDeclarationVerificationTarget as Target, LeanWorkerSorryPolicy,
        };
        let root = self.resolve(&hint)?;
        let meta = self.meta(&root)?;
        let file = PathBuf::from(&options.file_label);
        if !file.starts_with(&root)
            || file.canonicalize()? != file
            || file.extension().and_then(|x| x.to_str()) != Some("lean")
        {
            return Err(invalid("unapproved verification path"));
        }
        self.ensure_read(&root).await?;
        let (setup, reader, activity) = self
            .file_reader_with_setup(&hint, &meta, &options.file_label, &request.source)
            .await?;
        let options = options.lean_options(setup.options.clone());
        let roots = &reader.roots;
        let targets = request
            .targets
            .iter()
            .map(|t| match &t.target {
                Target::Name { name } => Ok(serde_json::json!({"id":t.id,"name":name})),
                Target::Span { .. } => Err(invalid("expected named declaration")),
                _ => Err(invalid("unsupported verification target")),
            })
            .collect::<Result<Vec<_>>>()?;
        let facts = crate::envelope::ElaborationOptions::from_worker(&options);
        let body = serde_json::to_vec(&serde_json::json!({
            "source": request.source, "file": options.file_label,
            "roots": roots, "imports": imports, "targets": targets,
            "allowSorry": matches!(request.sorry_policy, LeanWorkerSorryPolicy::Allow),
            "reportAxioms": request.report_axioms,
            "perFieldBytes": request.budgets.per_field_bytes,
            "totalBytes": request.budgets.total_bytes,
            "namespaceContext": options.namespace_context,
            "optionsJson": lean_toolchain::lean_options_json(&options.lean_options),
            "heartbeats": options.heartbeat_limit,
            "diagnosticBytes": options.diagnostic_byte_limit,
            "expectedRuntimeVersion": selected_lean_version(&meta.toolchain),
        }))
        .map_err(invalid)?;
        let entry = {
            use crate::tools::proof_action::LeanVerifyDetail;
            let selection = match detail {
                LeanVerifyDetail::Compact => "private abbrev verifySetup := verifyDeclarationBatchCompactWithSetup\n",
                LeanVerifyDetail::Full => "private abbrev verifySetup := verifyDeclarationBatchWithSetup\n",
            };
            [
                include_str!("../lake/VerifyCommon.lean"),
                selection,
                "private abbrev verifySetupReportRuntime := true\n",
                include_str!("../lake/VerifySetup.lean"),
            ]
            .concat()
        };
        let module_setup_sha256 = crate::digest::sha256_hex(&setup.bytes);
        let artifact_identity_sha256 = setup.artifact_identity_sha256(&reader.shim_identity_sha256)?;
        let lean_executable = reader.sysroot.join("bin/lean");
        let request_sha256 = crate::digest::sha256_hex(&body);
        let source_sha256 = crate::digest::sha256_hex(request.source.as_bytes());
        let bytes = self
            .run_file_driver(&root, reader, activity, setup, entry, body)
            .await?;
        let mut runtime = RuntimeFacts::default();
        let mut freshness = reader_freshness(&meta, freshness);
        let frame = decode_setup_verdict(&bytes, &freshness.lean_toolchain)?;
        if matches!(frame.outcome, LeanWorkerDeclarationVerificationBatchResult::Ok { .. }) {
            runtime.cli_file_verification = Some(Box::new(crate::envelope::CliFileVerification {
                schema: "lean-host-cli-file-verification-v1",
                server_instance_id: self.instance_id(),
                project_root: root,
                source_file: file,
                lean_toolchain: freshness.lean_toolchain.clone(),
                runtime_lean_version: frame.runtime_lean_version,
                lean_executable,
                source_sha256,
                module_setup_sha256,
                artifact_identity_sha256,
                request_sha256,
                data_sha256: String::new(),
            }));
        }
        let value = frame.outcome;
        if let LeanWorkerDeclarationVerificationBatchResult::Ok { results, .. }
        | LeanWorkerDeclarationVerificationBatchResult::MissingImports { results, .. } = &value
            && (results.len() != request.targets.len()
                || results
                    .iter()
                    .zip(&request.targets)
                    .any(|(row, target)| row.id != target.id || row.target != target.target))
        {
            return Err(invalid("verification verdict does not match requested targets"));
        }
        freshness.elaboration_options = Some(facts);
        Ok(BrokerCall {
            value,
            runtime,
            freshness,
        })
    }

    /// Run the Lean driver `entry` on request `body` with the module setup of
    /// `setup` and the installed shims, as one short run.
    async fn run_file_driver(
        &self,
        root: &Path,
        reader: PreparedReader,
        activity: super::control::PreparationActivityGuard,
        setup: super::module_setup::StagedSetup,
        entry: String,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let PreparedReader { sysroot, shims, .. } = reader;
        crate::lake_options::reader_output(
            "Lean file driver",
            root,
            crate::cancellation::CallCancellation::current()
                .remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.lean_runs(),
            Some(Box::new(activity)),
            move |assets| {
                let driver = assets.join("FileDriver.lean");
                fs::write(assets.join("module.setup.json"), setup.bytes)?;
                fs::write(&driver, entry)?;
                let data = assets.join("request.json");
                fs::write(&data, body)?;
                Ok(crate::lean_run::Program {
                    command: vec![
                        sysroot.join("bin/lean").into_os_string(),
                        "-j1".into(),
                        "--run".into(),
                        driver.into_os_string(),
                        data.into_os_string(),
                    ],
                    environment: vec![
                        (
                            "LEAN_PATH".into(),
                            std::env::join_paths(shims).map_err(io::Error::other)?,
                        ),
                        ("LEAN_SYSROOT".into(), sysroot.into_os_string()),
                    ],
                    cwd: assets.into(),
                    max_stdout_bytes: crate::lean_run::OUTPUT_LIMIT,
                    stdin: None,
                })
            },
        )
        .await
        .map_err(|e| e.error)
    }

    /// The declaration outline of `file`'s current source.
    pub(crate) async fn file_outline(
        &self,
        hint: &ProjectHint,
        file: &str,
        source: &str,
        imports: Vec<String>,
        budgets: lean_rs_worker_parent::LeanWorkerOutputBudgets,
    ) -> Result<super::CachedBrokerCall<lean_rs_worker_parent::LeanWorkerModuleQueryBatchOutcome>> {
        let root = self.resolve(hint)?;
        let meta = self.meta(&root)?;
        let (setup, reader, activity) = self.file_reader_with_setup(hint, &meta, file, source).await?;
        let options = LeanWorkerElabOptions::new()
            .file_label(file)
            .lean_options(setup.options.clone());
        let body = serde_json::to_vec(&serde_json::json!({
            "source": source, "file": file, "perFieldBytes": budgets.per_field_bytes,
            "totalBytes": budgets.total_bytes, "namespaceContext": options.namespace_context,
            "optionsJson": lean_toolchain::lean_options_json(&options.lean_options),
            "heartbeats": options.heartbeat_limit, "diagnosticBytes": options.diagnostic_byte_limit
        }))
        .map_err(invalid)?;
        let entry = [
            include_str!("../lake/VerifyCommon.lean"),
            "private def projectFile (_ : Option Json) := declarationOutlineWithSetup\nprivate def extraItem (_ : ModuleQueryBatchItem) : Option Json := none\n",
            include_str!("../lake/OutlineSetup.lean"),
        ]
        .concat();
        let bytes = self
            .run_file_driver(&root, reader, activity, setup, entry, body)
            .await?;
        let mut result: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        let object = result
            .as_object_mut()
            .ok_or_else(|| invalid("invalid supervised file result"))?;
        object.insert(
            "facts".into(),
            serde_json::to_value(lean_rs_worker_parent::LeanWorkerModuleQueryCacheFacts::uncached(
                bytes.len() as u64,
            ))
            .map_err(invalid)?,
        );
        let value = serde_json::from_value(result).map_err(invalid)?;
        let mut freshness = reader_freshness(&meta, imports);
        freshness.elaboration_options = Some(crate::envelope::ElaborationOptions::from_worker(&options));
        Ok(super::CachedBrokerCall {
            freshly_processed: true,
            value,
            runtime: RuntimeFacts::default(),
            freshness,
        })
    }

    /// The diagnostics of `source`, the current text of `file`, as `lean
    /// --json` prints them when it checks that text with the module setup that
    /// `lake setup-file` prints. Lean then runs its own frontend over the whole
    /// file with Lake's imports, plugins and options, so the list holds what a
    /// Lake build of the file prints. Lean leaves out silent messages, such as
    /// the syntax records the lean-fmt plugin logs.
    pub(crate) async fn file_diagnostics(
        &self,
        hint: &ProjectHint,
        file: &str,
        source: &str,
        imports: Vec<String>,
        budgets: lean_rs_worker_parent::LeanWorkerOutputBudgets,
    ) -> Result<BrokerCall<crate::projections::ElabFailure>> {
        let root = self.resolve(hint)?;
        let meta = self.meta(&root)?;
        let (setup, reader, activity) = self.file_reader_with_setup(hint, &meta, file, source).await?;
        let options = LeanWorkerElabOptions::new()
            .file_label(file)
            .lean_options(setup.options.clone());
        let PreparedReader { sysroot, .. } = reader;
        let (cwd, path, snapshot) = (root.clone(), PathBuf::from(file), source.as_bytes().to_vec());
        let output = crate::lake_options::reader_run(
            "Lean file diagnostics",
            &root,
            crate::cancellation::CallCancellation::current()
                .remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.lean_runs(),
            Some(Box::new(activity)),
            // `lean` exits 1 when the file has an error.
            &[0, 1],
            move |assets| {
                let setup_file = assets.join("module.setup.json");
                fs::write(&setup_file, setup.bytes)?;
                Ok(crate::lean_run::Program {
                    command: vec![
                        sysroot.join("bin/lean").into_os_string(),
                        "-j1".into(),
                        "--json".into(),
                        "--setup".into(),
                        setup_file.into_os_string(),
                        // With a file name, `--stdin` keeps the name and reads
                        // the text from stdin, so Lean checks the bytes the
                        // reply hashes.
                        "--stdin".into(),
                        path.into_os_string(),
                    ],
                    environment: Vec::new(),
                    // Lake runs `lean` from the project root.
                    cwd,
                    max_stdout_bytes: crate::lean_run::OUTPUT_LIMIT,
                    stdin: Some(snapshot),
                })
            },
        )
        .await
        .map_err(|failure| failure.error)?;
        let value = lean_messages(&output, file, &budgets)?;
        let mut freshness = reader_freshness(&meta, imports);
        freshness.elaboration_options = Some(crate::envelope::ElaborationOptions::from_worker(&options));
        Ok(BrokerCall {
            value,
            runtime: RuntimeFacts::default(),
            freshness,
        })
    }
}

/// One message as `lean --json` prints it: Lean's `SerialMessage`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeanMessage {
    pos: LeanPosition,
    #[serde(default)]
    end_pos: Option<LeanPosition>,
    severity: String,
    #[serde(default)]
    caption: String,
    data: String,
}

#[derive(Clone, Copy, Deserialize)]
struct LeanPosition {
    line: u32,
    column: u32,
}

/// One row for each line `lean --json` printed; a line that is not a message,
/// such as a print from an initializer, becomes an information row without a
/// position. When the byte budget cannot hold every row, errors come first,
/// then warnings, then the rest, and the list is marked truncated.
fn lean_messages(
    output: &crate::lake_options::ReaderOutput,
    file: &str,
    budgets: &lean_rs_worker_parent::LeanWorkerOutputBudgets,
) -> Result<crate::projections::ElabFailure> {
    use crate::projections::{CoordinateSpace, Diagnostic, ElabFailure, Position, Severity, SourceRange};
    let stdout = std::str::from_utf8(&output.stdout).map_err(invalid)?;
    let mut rows = Vec::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(message) = serde_json::from_str::<LeanMessage>(line) else {
            rows.push(Diagnostic {
                severity: Severity::Info,
                message: line.to_owned(),
                coordinate_space: CoordinateSpace::Unknown,
                position: None,
                original_range: None,
                synthetic_range: None,
                file: None,
                coordinate_note: Some("Lean printed this line outside its message log".to_owned()),
            });
            continue;
        };
        let severity = match message.severity.as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            "information" => Severity::Info,
            other => return Err(invalid(format!("Lean sent a message of unknown severity {other:?}"))),
        };
        let (start, end) = (message.pos, message.end_pos.unwrap_or(message.pos));
        rows.push(Diagnostic {
            severity,
            // Lean's own text output puts a caption on the line before the message.
            message: if message.caption.is_empty() {
                message.data
            } else {
                format!("{}:\n{}", message.caption, message.data)
            },
            coordinate_space: CoordinateSpace::OriginalSource,
            position: Some(Position {
                line: start.line,
                column: start.column,
                end_line: message.end_pos.map(|p| p.line),
                end_column: message.end_pos.map(|p| p.column),
            }),
            original_range: Some(SourceRange {
                file: file.to_owned(),
                start_line: start.line,
                start_column: start.column,
                end_line: end.line,
                end_column: end.column,
            }),
            synthetic_range: None,
            file: Some(file.to_owned()),
            coordinate_note: None,
        });
    }
    let has_error = rows.iter().any(|row| matches!(row.severity, Severity::Error));
    if has_error != (output.exit_code != 0) {
        return Err(invalid(format!(
            "lean exited with code {} after printing {} error: {}",
            output.exit_code,
            if has_error { "an" } else { "no" },
            output.stderr.trim()
        )));
    }
    rows.sort_by_key(|row| match row.severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });
    let per_field = usize::try_from(budgets.per_field_bytes).unwrap_or(usize::MAX);
    let total = usize::try_from(budgets.total_bytes).unwrap_or(usize::MAX);
    let (mut spent, mut truncated, mut diagnostics) = (0_usize, false, Vec::new());
    for mut row in rows {
        if row.message.len() > per_field {
            let mut cut = per_field;
            while !row.message.is_char_boundary(cut) {
                cut = cut.saturating_sub(1);
            }
            row.message.truncate(cut);
            truncated = true;
        }
        spent = spent.saturating_add(row.message.len());
        if spent > total {
            truncated = true;
            break;
        }
        diagnostics.push(row);
    }
    Ok(ElabFailure { diagnostics, truncated })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::projections::Severity;

    const FILE: &str = "/p/A.lean";
    const ERROR: &str = r#"{"fileName":"/p/A.lean","pos":{"line":3,"column":2},"endPos":{"line":3,"column":7},"keepFullRange":false,"severity":"error","isSilent":false,"caption":"","data":"unknown identifier 'x'","kind":"lean.unknownIdentifier"}"#;
    const INFO: &str = r#"{"fileName":"/p/A.lean","pos":{"line":1,"column":0},"endPos":null,"keepFullRange":false,"severity":"information","isSilent":false,"caption":"note","data":"hi","kind":"[anonymous]"}"#;

    fn output(exit_code: i32, lines: &[&str]) -> crate::lake_options::ReaderOutput {
        crate::lake_options::ReaderOutput {
            exit_code,
            stdout: lines.join("\n").into_bytes(),
            stderr: "trace".to_owned(),
        }
    }

    fn budgets(per_field_bytes: u32, total_bytes: u32) -> lean_rs_worker_parent::LeanWorkerOutputBudgets {
        lean_rs_worker_parent::LeanWorkerOutputBudgets {
            per_field_bytes,
            total_bytes,
        }
    }

    #[test]
    fn each_lean_message_becomes_one_row_at_lean_s_position() {
        let failure = lean_messages(&output(1, &[INFO, ERROR, "", "printed"]), FILE, &budgets(100, 1000)).unwrap();
        assert!(!failure.truncated);
        let [error, info, printed] = &failure.diagnostics[..] else {
            panic!("{failure:?}");
        };
        assert!(matches!(error.severity, Severity::Error) && error.message == "unknown identifier 'x'");
        let position = error.position.as_ref().unwrap();
        assert_eq!(
            (position.line, position.column, position.end_line, position.end_column),
            (3, 2, Some(3), Some(7))
        );
        assert_eq!(error.original_range.as_ref().unwrap().file, FILE);
        assert!(matches!(info.severity, Severity::Info) && info.message == "note:\nhi");
        assert_eq!(info.position.as_ref().unwrap().end_line, None);
        assert!(printed.message == "printed" && printed.position.is_none() && printed.coordinate_note.is_some());
    }

    #[test]
    fn an_exit_code_that_disagrees_with_the_messages_decides_nothing() {
        let error = lean_messages(&output(0, &[ERROR]), FILE, &budgets(100, 1000)).unwrap_err();
        assert!(error.to_string().contains("code 0 after printing an error"), "{error}");
        let error = lean_messages(&output(1, &[INFO]), FILE, &budgets(100, 1000)).unwrap_err();
        assert!(
            error.to_string().contains("code 1 after printing no error: trace"),
            "{error}"
        );
    }

    #[test]
    fn a_short_budget_keeps_errors_before_information_and_marks_the_cut() {
        // The error's 22 bytes fit in 25; the information's 8 more do not.
        let failure = lean_messages(&output(1, &[INFO, ERROR]), FILE, &budgets(100, 25)).unwrap();
        assert!(failure.truncated);
        assert_eq!(failure.diagnostics.len(), 1);
        assert!(matches!(failure.diagnostics[0].severity, Severity::Error));
        let failure = lean_messages(&output(1, &[ERROR]), FILE, &budgets(7, 1000)).unwrap();
        assert!(failure.truncated && failure.diagnostics[0].message == "unknown");
    }
}
