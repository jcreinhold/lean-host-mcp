//! One-shot checks of current source against the operator-selected build products.
use super::{
    BrokerCall, Duration, LeanWorkerDeclarationVerificationBatchRequest, LeanWorkerDeclarationVerificationBatchResult,
    LeanWorkerElabOptions, Path, PathBuf, ProjectBroker, ProjectHint, Result, RuntimeFacts, ServerError,
    freshness_from_meta,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, io};

#[derive(Deserialize)]
struct Selection {
    plan: Plan,
    products: Vec<Vec<Option<String>>>,
}
#[derive(Deserialize)]
struct Plan {
    project: PathBuf,
    roots: Vec<PathBuf>,
    release: PathBuf,
    modules: Vec<(String, PathBuf)>,
}

fn invalid(message: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(message.to_string())
}
fn verify_hash(path: &Path, expected: &str) -> Result<()> {
    if path.canonicalize()? != path || !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("noncanonical prepared file"));
    }
    let mut h = Sha256::new();
    use std::io::Read as _;
    let mut f = fs::File::open(path)?;
    let mut buf = [0u8; 8192];
    loop {
        let count = f.read(&mut buf)?;
        if count == 0 {
            break;
        }
        h.update(buf.get(..count).ok_or_else(|| invalid("invalid read count"))?);
    }

    use std::fmt::Write as _;
    let mut actual = String::with_capacity(64);
    for byte in h.finalize() {
        write!(actual, "{byte:02x}").map_err(invalid)?;
    }
    if actual != expected {
        return Err(invalid("prepared verification artifact changed"));
    }
    Ok(())
}

pub(crate) enum FileProjection {
    Outline,
    Diagnostics,
}

pub(super) struct PreparedReader {
    pub(super) custody: crate::artifact_reads::ArtifactReadGuard,
    pub(super) sysroot: PathBuf,
    pub(super) shims: Vec<PathBuf>,
    pub(super) domains: Vec<PathBuf>,
    pub(super) shim_guards: lean_toolchain::shared_writer::LocalArtifactRead,
    pub(super) roots: Vec<PathBuf>,
}

impl ProjectBroker {
    pub(super) fn prepared_reader(
        root: &Path,
        meta: &crate::lake_meta::LakeProjectMeta,
        input: &str,
        imports: &[String],
    ) -> Result<PreparedReader> {
        let selection: Selection = serde_json::from_str(input).map_err(invalid)?;
        let plan = selection.plan;
        if plan.project != root {
            return Err(invalid("unexpected prepared project"));
        }
        let reader = Self::installed_reader(root, meta)?;
        let PreparedReader { sysroot, shims, .. } = &reader;
        if plan.release != sysroot.join("lib/lean") || selection.products.len() != plan.modules.len() {
            return Err(invalid("incomplete prepared selection"));
        }
        let project_domains = lean_toolchain::shared_writer::project_artifact_roots(root)?;
        lean_host_mcp_worker::prepared_project::PreparedGeneration::check_identity(input, &project_domains)
            .map_err(invalid)?;
        let mut expected: Vec<_> = project_domains.iter().map(|p| p.join(".lake/build/lib/lean")).collect();
        expected.extend(shims.clone());
        expected.push(plan.release.clone());
        let mut ordered = plan.roots.clone();
        ordered.sort();
        expected.sort();
        if ordered != expected {
            return Err(invalid("prepared roots differ from installed domains"));
        }
        if lean_toolchain::olean_candidate(&plan.roots, "Init", |p| fs::metadata(p)).map_err(invalid)?
            != Some(plan.release.join("Init.olean"))
        {
            return Err(invalid("prepared roots shadow builtin Init"));
        }
        for ((name, domain), parts) in plan.modules.iter().zip(&selection.products) {
            let base = domain.join(".lake/build/lib/lean").join(name.replace('.', "/"));
            if !project_domains.contains(domain)
                || parts.len() != 5
                || parts.first().and_then(Option::as_ref).is_none()
                || lean_toolchain::olean_candidate(&plan.roots, name, |p| fs::metadata(p)).map_err(invalid)?
                    != Some(base.with_extension("olean"))
            {
                return Err(invalid("prepared module resolution changed"));
            }
            for (ext, hash) in ["olean", "olean.server", "olean.private", "ir.sig", "ir"]
                .into_iter()
                .zip(parts)
            {
                let path = base.with_extension(ext);
                if let Some(hash) = hash {
                    verify_hash(&path, hash)?;
                } else if !matches!(fs::symlink_metadata(path),Err(e) if e.kind()==io::ErrorKind::NotFound) {
                    return Err(invalid("unapproved prepared module part"));
                }
            }
        }
        if imports
            .iter()
            .any(|n| n != "Init" && !plan.modules.iter().any(|(name, _)| name == n))
        {
            return Err(invalid("unsupported import outside prepared project"));
        }
        Ok(PreparedReader {
            roots: plan.roots,
            ..reader
        })
    }

    pub(super) fn installed_reader(root: &Path, meta: &crate::lake_meta::LakeProjectMeta) -> Result<PreparedReader> {
        let custody = crate::artifact_reads::ArtifactReadGuard::for_project(root).map_err(invalid)?;
        let shims = VerifierShims::select(meta)?;
        let shim_guards = lean_toolchain::shared_writer::LocalArtifactRead::try_acquire(&shims.domains)?;
        shims.validate()?;
        Ok(PreparedReader {
            custody,
            sysroot: shims.sysroot,
            shims: shims.paths,
            domains: shims.domains,
            shim_guards,
            roots: Vec::new(),
        })
    }
}

pub(crate) struct VerifierShims {
    #[cfg(all(unix, feature = "managed-native-store"))]
    pub(crate) worker: PathBuf,
    pub(crate) sysroot: PathBuf,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) domains: Vec<PathBuf>,
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
            #[cfg(all(unix, feature = "managed-native-store"))]
            worker,
            sysroot,
            domains: installed.prepared.artifact_roots.iter().map(PathBuf::from).collect(),
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
        let root = self.native_root(&hint)?;
        let meta = self.meta(&root)?;
        let file = PathBuf::from(&options.file_label);
        if !file.starts_with(&root)
            || file.canonicalize()? != file
            || file.extension().and_then(|x| x.to_str()) != Some("lean")
        {
            return Err(invalid("unapproved verification path"));
        }
        let (options, reader, setup, activity) = if let Some(selection) = self.prepared_generation() {
            let options = self.resolve_options(&meta, options).await?;
            let activity = self.preparation_activity(&root).await?;
            let reader = Self::prepared_reader(&root, &meta, selection, &imports)?;
            (options, reader, None, activity)
        } else {
            let (setup, reader, activity) = self
                .file_reader_with_setup(&hint, &meta, &options.file_label, &request.source)
                .await?;
            (
                options.lean_options(setup.options.clone()),
                reader,
                Some(setup),
                activity,
            )
        };
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
        }))
        .map_err(invalid)?;
        let entry = if setup.is_some() {
            use crate::tools::proof_action::LeanVerifyDetail;
            let selection = match detail {
                LeanVerifyDetail::Compact => "private abbrev verifySetup := verifyDeclarationBatchCompactWithSetup\n",
                LeanVerifyDetail::Full => "private abbrev verifySetup := verifyDeclarationBatchWithSetup\n",
            };
            [
                include_str!("../lake/VerifyCommon.lean"),
                selection,
                include_str!("../lake/VerifySetup.lean"),
            ]
            .concat()
        } else {
            [
                include_str!("../lake/VerifyCommon.lean"),
                include_str!("../lake/VerifyBatch.lean"),
            ]
            .concat()
        };
        let bytes = self.run_file_driver(root, reader, activity, setup, entry, body).await?;
        let value = serde_json::from_slice(&bytes).map_err(invalid)?;
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
        let mut freshness = freshness_from_meta(&meta, freshness);
        freshness.elaboration_options = Some(facts);
        Ok(BrokerCall {
            value,
            runtime: RuntimeFacts::default(),
            freshness,
        })
    }

    async fn run_file_driver(
        &self,
        root: PathBuf,
        reader: PreparedReader,
        activity: super::control::PreparationActivityGuard,
        setup: Option<super::module_setup::StagedSetup>,
        entry: String,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let PreparedReader {
            custody,
            sysroot,
            shims,
            domains,
            shim_guards,
            ..
        } = reader;
        crate::lake_options::reader_output(
            "Lean file driver",
            root.clone(),
            Duration::from_millis(self.request_timeout_millis()),
            self.config.max_projects.get(),
            custody,
            None,
            Some(Box::new((activity, shim_guards))),
            move |assets| {
                let driver = assets.join("FileDriver.lean");
                if let Some(setup) = setup {
                    fs::write(assets.join("module.setup.json"), setup.bytes)?;
                }
                fs::write(&driver, entry)?;
                let data = assets.join("request.json");
                fs::write(&data, body)?;
                Ok(crate::native_reader::NativeReaderProgram {
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
                    artifact_roots: domains,
                })
            },
        )
        .await
        .map_err(|e| e.error)
    }

    pub(crate) async fn file_projection(
        &self,
        hint: &ProjectHint,
        file: &str,
        source: &str,
        imports: Vec<String>,
        projection: FileProjection,
        budgets: lean_rs_worker_parent::LeanWorkerOutputBudgets,
    ) -> Result<super::CachedBrokerCall<lean_rs_worker_parent::LeanWorkerModuleQueryBatchOutcome>> {
        let root = self.native_root(hint)?;
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
            match projection {
                FileProjection::Outline => "private abbrev projectFile := declarationOutlineWithSetup\n",
                FileProjection::Diagnostics => "private abbrev projectFile := fileDiagnosticsWithSetup\n",
            },
            include_str!("../lake/OutlineSetup.lean"),
        ]
        .concat();
        let bytes = self
            .run_file_driver(root, reader, activity, Some(setup), entry, body)
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
        let mut freshness = freshness_from_meta(&meta, imports);
        freshness.elaboration_options = Some(crate::envelope::ElaborationOptions::from_worker(&options));
        Ok(super::CachedBrokerCall {
            freshly_processed: true,
            value,
            runtime: RuntimeFacts::default(),
            freshness,
        })
    }
}
