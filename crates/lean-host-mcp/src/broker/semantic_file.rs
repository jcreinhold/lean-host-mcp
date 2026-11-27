//! Semantic proof search for a goal in one source file. A native reader runs
//! the semantic feature driver with the module setup that `lake setup-file`
//! prints for that file.
use super::verify_file::PreparedReader;
use super::{BrokerCall, ProjectBroker, ProjectHint, Result, RuntimeFacts, ServerError, reader_freshness};
use crate::cancellation::CallCancellation;
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

fn invalid(error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(error.to_string())
}

impl ProjectBroker {
    /// Rank declarations that may close the goal at one proof position.
    ///
    /// # Errors
    ///
    /// Returns resolution, `lake setup-file`, semantic runtime build, or
    /// reader failures.
    pub(crate) async fn semantic_proof_search(
        &self,
        hint: ProjectHint,
        freshness: Vec<String>,
        request: SemanticProofSearchRequest,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let root = self.resolve(&hint)?;
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
        let build_cancellation = cancellation.clone();
        let label = meta.toolchain.clone();
        let prefix = sysroot.clone();
        let (activity, runtime) = tokio::task::spawn_blocking(move || {
            (
                activity,
                crate::project::semantic_runtime(&label, &prefix, &build_cancellation),
            )
        })
        .await
        .map_err(invalid)?;
        let runtime = runtime.map_err(|failure| {
            // The build ran under the shared writer. Unless the writer reports
            // that it never started or stopped, its command may still run.
            if !failure.closed() {
                cancellation.mark_uncertain();
            }
            invalid(failure)
        })?;
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
        let bootstrap = select_semantic_runtime(|| Ok(runtime_guard.validate()?), &package, &image)?;
        let body = request.reader_input(&roots)?;
        domains.push(package);
        let bytes = crate::lake_options::reader_output(
            "Lean semantic feature reader",
            root,
            cancellation.remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.config.max_workers.get(),
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
                    stdin: None,
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

/// Return the directory that holds the semantic runtime's compiled Lean
/// modules, checking that it lies in the package the read turn covers.
fn select_semantic_runtime(validate_custody: impl Fn() -> Result<()>, package: &Path, image: &Path) -> Result<PathBuf> {
    validate_custody()?;
    if !image.starts_with(package) {
        return Err(invalid("semantic runtime image outside its package"));
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
