//! Semantic proof search for a goal in one source file. A short run
//! ([`crate::lean_run`]) runs the semantic feature driver with the module
//! setup that `lake setup-file` prints for that file.
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
    /// Returns resolution, `lake setup-file`, semantic runtime, or short-run
    /// failures.
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
        reader.roots.push(reader.sysroot.join("lib/lean"));
        let PreparedReader { sysroot, roots, .. } = reader;
        let cancellation = CallCancellation::current();
        let label = meta.toolchain.clone();
        let prefix = sysroot.clone();
        // Opening asks Lake whether install built the runtime; it builds
        // nothing.
        let runtime = tokio::task::spawn_blocking(move || crate::project::semantic_runtime(&label, &prefix))
            .await
            .map_err(invalid)??;
        let bootstrap = select_semantic_runtime(runtime.package_root(), runtime.module_dir())?;
        let body = request.reader_input(&roots)?;
        let bytes = crate::lake_options::reader_output(
            "Lean semantic feature reader",
            &root,
            cancellation.remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.lean_runs(),
            Some(Box::new(activity)),
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
                Ok(crate::lean_run::Program {
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
                    max_stdout_bytes: crate::lean_run::OUTPUT_LIMIT,
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
/// modules, checking that it lies in the runtime's package.
fn select_semantic_runtime(package: &Path, modules: &Path) -> Result<PathBuf> {
    if !modules.starts_with(package) {
        return Err(invalid("semantic runtime modules outside their package"));
    }
    if !modules.join("LeanSemanticSearch/Capability.olean").is_file() {
        return Err(invalid("missing compiled semantic capability"));
    }
    Ok(modules.to_owned())
}
