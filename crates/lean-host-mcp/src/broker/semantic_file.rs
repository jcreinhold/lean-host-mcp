//! Semantic proof search for a goal in one source file. A short run
//! ([`crate::lean_run`]) runs search's driver with the module setup that
//! `lake setup-file` prints for that file.
use super::verify_file::PreparedReader;
use super::{BrokerCall, ProjectBroker, ProjectHint, Result, RuntimeFacts, short_run_freshness};
use crate::cancellation::CallCancellation;
use crate::error::invalid;
use crate::semantic_search::{SemanticProofSearchRequest, SemanticProofSearchResult};

use std::{fs, time::Duration};

impl ProjectBroker {
    /// Rank declarations that may close the goal at one proof position.
    ///
    /// The goal's module is the one Lake names for the file, and the search
    /// reads that module's declarations first unless the file lies outside
    /// every library.
    ///
    /// # Errors
    ///
    /// Returns resolution, `lake setup-file`, semantic runtime, or short-run
    /// failures, and the contract's refusal of the driver's reply.
    pub(crate) async fn semantic_proof_search(
        &self,
        hint: ProjectHint,
        freshness: Vec<String>,
        mut request: SemanticProofSearchRequest,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let root = self.resolve(&hint)?;
        let meta = self.meta(&root)?;
        let file = request
            .goal
            .file_label
            .as_deref()
            .ok_or_else(|| invalid("missing semantic source file"))?;
        let (setup, mut reader, activity) = self
            .file_reader_with_setup(&hint, &meta, file, &request.goal.source_text)
            .await?;
        request.goal.module.clone_from(&setup.module);
        request.candidate_modules.retain(|module| *module != setup.module);
        if setup.module != super::module_setup::EXTERNAL_MODULE {
            request.candidate_modules.insert(0, setup.module.clone());
        }
        reader.roots = crate::lake_meta::manifest_artifact_roots(&root);
        reader.roots.push(reader.sysroot.join("lib/lean"));
        let PreparedReader { sysroot, roots, .. } = reader;
        let cancellation = CallCancellation::current();
        let label = meta.toolchain.clone();
        let prefix = sysroot.clone();
        let project = root.clone();
        // Opening asks Lake whether install built the runtime and where its
        // modules are; it builds nothing.
        let runtime = tokio::task::spawn_blocking(move || crate::project::semantic_runtime(&label, &prefix, &project))
            .await
            .map_err(invalid)??;
        let limit = request.limit;
        let body = serde_json::to_vec(&request.into_driver_request(roots)).map_err(invalid)?;
        let driver = runtime.driver().to_owned();
        let lean_path = std::env::join_paths(runtime.lean_path()).map_err(invalid)?;
        let stdout = super::reader::reader_output(
            "Lean semantic search driver",
            &root,
            cancellation.remaining_or(Duration::from_millis(self.request_timeout_millis())),
            self.lean_runs(),
            Some(Box::new(activity)),
            move |assets| {
                let setup_file = assets.join("module.setup.json");
                fs::write(&setup_file, setup.bytes)?;
                let request_file = assets.join("request.json");
                fs::write(&request_file, body)?;
                Ok(crate::lean_run::Program {
                    command: vec![
                        sysroot.join("bin/lean").into_os_string(),
                        "--run".into(),
                        driver.into_os_string(),
                        setup_file.into_os_string(),
                        request_file.into_os_string(),
                    ],
                    environment: vec![
                        ("LEAN_PATH".into(), lean_path),
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
            value: crate::semantic_search::rank_driver_reply(&stdout, limit)?,
            runtime: RuntimeFacts::default(),
            freshness: short_run_freshness(&meta, freshness),
        })
    }
}
