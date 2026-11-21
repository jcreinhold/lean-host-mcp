//! Supervised semantic adapter for an explicitly selected prepared project.
use super::verify_file::PreparedReader;
use super::{
    BrokerCall, ProjectBroker, ProjectHint, Result, RuntimeFacts, SemanticProofSearchRequest,
    SemanticProofSearchResult, ServerError, freshness_from_meta,
};
use crate::cancellation::CallCancellation;
use std::{fs, io, path::Path, time::Duration};

fn invalid(error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(error.to_string())
}

impl ProjectBroker {
    pub(super) async fn semantic_file(
        &self,
        hint: ProjectHint,
        imports: Vec<String>,
        freshness: Vec<String>,
        request: SemanticProofSearchRequest,
        input: Option<&str>,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let root = self.native_root(&hint)?;
        let meta = self.meta(&root)?;
        let (reader, setup, activity) = if let Some(input) = input {
            let activity = self.preparation_activity(&root).await?;
            let mut selected = imports;
            selected.extend(request.candidate_modules.iter().cloned());
            (Self::prepared_reader(&root, &meta, input, &selected)?, None, activity)
        } else {
            let file = request
                .goal
                .file_label
                .as_deref()
                .ok_or_else(|| invalid("missing semantic source file"))?;
            let (setup, mut reader, activity) = self
                .file_reader_with_setup(&hint, &meta, file, &request.goal.source_text)
                .await?;
            reader.roots = crate::lake_meta::artifact_roots(&root);
            if reader.roots.iter().any(|p| !reader.custody.covers_artifact_root(p)) {
                return Err(invalid("semantic declaration root outside held project domains"));
            }
            reader.roots.push(reader.sysroot.join("lib/lean"));
            (reader, Some(setup), activity)
        };
        let PreparedReader {
            custody,
            sysroot,
            mut domains,
            shim_guards,
            roots,
            ..
        } = reader;
        let cancellation = CallCancellation::current();
        let label = meta.toolchain.clone();
        let prefix = sysroot.clone();
        let (activity, runtime) = tokio::task::spawn_blocking(move || {
            (
                activity,
                crate::project::semantic_runtime(&label, &prefix, &cancellation),
            )
        })
        .await
        .map_err(invalid)?;
        let runtime = runtime.map_err(invalid)?;
        let path = runtime
            .built
            .resolved_manifest_path()
            .map_err(invalid)?
            .canonicalize()?;
        let package = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == ".lake"))
            .and_then(Path::parent)
            .ok_or_else(|| invalid("missing semantic runtime package"))?
            .to_owned();
        // Dropping the build writer closes its input. Its observer can still be
        // reaping the helper when the completed build returns.
        let started = std::time::Instant::now();
        let budget = Duration::from_millis(self.request_timeout_millis());
        let runtime_guard = loop {
            match lean_toolchain::shared_writer::LocalArtifactRead::try_acquire(std::slice::from_ref(&package)) {
                Ok(guard) => break guard,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && started.elapsed() < budget => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let manifest = lean_toolchain::CapabilityManifest::read(&path).map_err(invalid)?;
        let image = runtime.built.dylib_path().map_err(invalid)?.canonicalize()?;
        if !image.starts_with(&package)
            || image != manifest.primary_dylib
            || manifest.module != lean_semantic_search_runtime::LIBRARY_NAME
            || manifest.package != runtime.provenance.materialized_package
            || runtime.provenance.runtime_source_digest != lean_semantic_search_runtime::RUNTIME_SOURCE_DIGEST
            || runtime.provenance.source_revision != lean_semantic_search_runtime::SOURCE_REVISION
            || runtime.provenance.toolchain_label != meta.toolchain
        {
            return Err(invalid("semantic runtime selection changed"));
        }
        let bootstrap = image
            .parent()
            .ok_or_else(|| invalid("missing semantic image directory"))?
            .join("lean")
            .canonicalize()?;
        if !bootstrap.join("LeanSemanticSearch/Capability.olean").is_file() {
            return Err(invalid("missing compiled semantic capability"));
        }
        let body = request.reader_input(&roots)?;
        domains.push(package);
        let bytes = crate::lake_options::discovery_output(
            root,
            Duration::from_millis(self.request_timeout_millis()),
            self.config.max_projects.get(),
            custody,
            None,
            Some(Box::new((activity, shim_guards, runtime_guard))),
            move |assets| {
                let driver = assets.join("SemanticFeatures.lean");
                let entry = if let Some(setup) = setup {
                    fs::write(assets.join("module.setup.json"), setup.bytes)?;
                    include_str!("../lake/SemanticSetup.lean")
                } else {
                    include_str!("../lake/SemanticPrepared.lean")
                };
                fs::write(&driver, [include_str!("../lake/SemanticFeatures.lean"), entry].concat())?;
                let data = assets.join("semantic.json");
                fs::write(&data, body)?;
                Ok(crate::native_reader::NativeReaderProgram {
                    command: vec![
                        sysroot.join("bin/lean").into_os_string(),
                        "--run".into(),
                        driver.into_os_string(),
                        data.into_os_string(),
                    ],
                    environment: vec![
                        (
                            "LEAN_PATH".into(),
                            std::env::join_paths([bootstrap]).map_err(io::Error::other)?,
                        ),
                        ("LEAN_SYSROOT".into(), sysroot.into_os_string()),
                    ],
                    cwd: assets.into(),
                    artifact_roots: domains,
                })
            },
        )
        .await
        .map_err(|e| e.error)?;
        Ok(BrokerCall {
            value: request.reader_result(&bytes)?,
            runtime: RuntimeFacts::default(),
            freshness: freshness_from_meta(&meta, freshness),
        })
    }
}
