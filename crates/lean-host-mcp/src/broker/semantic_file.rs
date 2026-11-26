//! Semantic proof search for a goal in one source file. A native reader runs
//! the semantic feature driver with the module setup that `lake setup-file`
//! prints for that file.
use super::verify_file::PreparedReader;
use super::{
    BrokerCall, ProjectBroker, ProjectHint, Result, RuntimeFacts, SemanticProofSearchRequest,
    SemanticProofSearchResult, ServerError, reader_freshness,
};
use crate::cancellation::CallCancellation;
use lean_semantic_search_runtime::SemanticSearchRuntimeProvenance;

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

fn invalid(error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(error.to_string())
}

impl ProjectBroker {
    pub(super) async fn semantic_file(
        &self,
        hint: ProjectHint,
        freshness: Vec<String>,
        request: SemanticProofSearchRequest,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let root = self.native_root(&hint)?;
        let meta = self.meta(&root)?;
        self.ensure_read(&root).await?;
        let file = request
            .goal
            .file_label
            .as_deref()
            .ok_or_else(|| invalid("missing semantic source file"))?;
        let (setup, mut reader, activity) = self
            .file_reader_with_setup(&hint, &meta, file, &request.goal.source_text)
            .await?;
        reader.roots = crate::lake_meta::manifest_artifact_roots(&root);
        if reader.roots.iter().any(|p| !reader.custody.covers_artifact_root(p)) {
            return Err(invalid("semantic declaration root outside held project domains"));
        }
        reader.roots.push(reader.sysroot.join("lib/lean"));
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
        let runtime_guard = self.read_when_admitted(std::slice::from_ref(&package)).await?;
        let image = runtime.built.dylib_path().map_err(invalid)?.canonicalize()?;
        let bootstrap = select_semantic_runtime(
            || Ok(runtime_guard.validate()?),
            &package,
            &path,
            &image,
            &meta.toolchain,
            &runtime.provenance,
        )?;
        let body = request.reader_input(&roots)?;
        domains.push(package);
        let bytes = crate::lake_options::reader_output(
            "Lean semantic feature reader",
            root,
            CallCancellation::current().remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.config.max_projects.get(),
            custody,
            None,
            Some(Box::new((activity, shim_guards, runtime_guard))),
            move |assets| {
                let driver = assets.join("SemanticFeatures.lean");
                fs::write(assets.join("module.setup.json"), setup.bytes)?;
                fs::write(
                    &driver,
                    [
                        include_str!("../lake/SemanticFeatures.lean"),
                        include_str!("../lake/SemanticSetup.lean"),
                    ]
                    .concat(),
                )?;
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
                    max_stdout_bytes: crate::native_reader::OUTPUT_LIMIT,
                })
            },
        )
        .await
        .map_err(|e| e.error)?;
        Ok(BrokerCall {
            value: request.reader_result(&bytes)?,
            runtime: RuntimeFacts::default(),
            freshness: reader_freshness(&meta, freshness),
        })
    }
}

/// Check the built semantic runtime against its builder provenance and return
/// the directory that holds its compiled Lean modules.
fn select_semantic_runtime(
    validate_custody: impl Fn() -> Result<()>,
    package: &Path,
    path: &Path,
    image: &Path,
    toolchain: &str,
    provenance: &SemanticSearchRuntimeProvenance,
) -> Result<PathBuf> {
    validate_custody()?;
    let manifest = lean_toolchain::CapabilityManifest::read(path).map_err(invalid)?;
    if !image.starts_with(package)
        || image != manifest.primary_dylib
        || manifest.module != lean_semantic_search_runtime::LIBRARY_NAME
        || manifest.package != provenance.materialized_package
        || provenance.runtime_source_digest != lean_semantic_search_runtime::RUNTIME_SOURCE_DIGEST
        || provenance.source_revision != lean_semantic_search_runtime::SOURCE_REVISION
        || provenance.toolchain_label != toolchain
    {
        return Err(invalid("semantic runtime selection changed"));
    }
    let bootstrap = semantic_bootstrap(image)?;
    if !bootstrap.join("LeanSemanticSearch/Capability.olean").is_file() {
        return Err(invalid("missing compiled semantic capability"));
    }
    validate_custody()?;
    Ok(bootstrap)
}

fn semantic_bootstrap(image: &Path) -> Result<PathBuf> {
    Ok(image
        .parent()
        .ok_or_else(|| invalid("missing semantic image directory"))?
        .join("lean")
        .canonicalize()?)
}
