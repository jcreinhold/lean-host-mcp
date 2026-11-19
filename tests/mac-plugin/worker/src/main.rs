use lean_rs::host_bridge::{
    HostCachedModuleBatch, HostModuleBatch, HostModuleQuery, HostSourceOptions, TrustedHostKernelRequest,
    TrustedHostTerm, TrustedModuleSource,
    module_requests::{DeclarationVerificationBatchRequest, DeclarationVerificationRequest, ProofAttemptRequest},
};
use lean_rs::module::{
    AuthorizedWorkerSnapshot, HostImportOptions, LeanLibrary, LeanModuleInitializer, LoadedHostArtifacts,
    TrustedHostCapability, TrustedHostImports,
};
use lean_rs::{LeanResult, LeanRuntime};
use lean_rs_host::host::LeanPreparedHostArtifacts;
use lean_rs_worker_child::{WorkerAuthority, WorkerManifestRead, WorkerSourceContext};
use std::path::Path;

mod custody;
use custody::{Held, Record, audit};
use lean_rs::host_bridge::module_requests::ModuleQuery;
use std::sync::{Arc, OnceLock};
struct HostTrial(OnceLock<Result<Arc<Held>, String>>);
impl HostTrial {
    fn deployment(&self) -> LeanResult<&Arc<Held>> {
        self.0
            .get_or_init(|| {
                Held::acquire(Record::selected())
                    .and_then(|held| {
                        held.verify()?;
                        Ok(Arc::new(held))
                    })
                    .map_err(|error| {
                        audit(&format!("refused {error}"));
                        error.to_string()
                    })
            })
            .as_ref()
            .map_err(|message| lean_rs::error::host_internal(message.clone()))
    }
}
fn denied() -> lean_rs::LeanError {
    lean_rs::error::host_internal("operation outside private host deployment")
}
fn ioerr(e: std::io::Error) -> lean_rs::LeanError {
    audit(&format!("refused {e}"));
    lean_rs::error::host_internal(e.to_string())
}
impl WorkerAuthority for HostTrial {
    fn validate_source_context(
        &self,
        _session: &WorkerSourceContext<'_, '_>,
        observed_roots: Option<&[String]>,
    ) -> LeanResult<()> {
        let held = self.deployment()?;
        observed::match_order(observed_roots, &held.record.expected_roots()).ok_or_else(denied)?;
        audit("validated-opened-roots");
        Ok(())
    }
    fn manifest_read(&self, path: &Path) -> LeanResult<WorkerManifestRead> {
        let _ = path;
        Err(denied())
    }
    fn worker(&self, _: &'static LeanRuntime, _: &Path, _: &[u8]) -> LeanResult<AuthorizedWorkerSnapshot<'static>> {
        Err(denied())
    }
    fn host(
        &self,
        runtime: &'static LeanRuntime,
        p: &LeanPreparedHostArtifacts,
    ) -> LeanResult<LoadedHostArtifacts<'static>> {
        let held = self.deployment()?;
        let d = &held.record;
        if p.manifest_path() != d.shim_manifest
            || p.manifest_json() != d.shim_json
            || p.artifact_roots() != d.shim_roots
            || p.shim_search_paths() != d.shim_paths
        {
            audit("refused prepared host descriptor");
            return Err(denied());
        }

        audit("authorize-host-load");
        // SAFETY: the build-selected SDK supplied its own compiled shim and full generated ABI.
        // Admission checked both manifests, the common interop image and all image bytes
        // under the retained deployment guard. This worker uses the pinned private release.
        let loaded = unsafe {
            LoadedHostArtifacts::load(
                runtime,
                p.manifest_path().into(),
                p.manifest_json().as_bytes().to_vec(),
                d.shim_roots.clone(),
                d.shim_paths.clone(),
                held.clone(),
            )
        }?;
        held.check_interop_visibility(true).map_err(ioerr)?;
        Ok(loaded)
    }
    fn imports<'cap>(
        &self,
        cap: &'cap TrustedHostCapability<'static>,
        paths: Vec<String>,
        names: Vec<String>,
        options: HostImportOptions,
    ) -> LeanResult<TrustedHostImports<'static, 'cap>> {
        let held = self.deployment()?;
        if cap.library_path() != held.record.host_image {
            return Err(denied());
        }
        held.record.check_order(&paths, &names, &options).map_err(ioerr)?;
        let root = held.record.project.join(".lake/build/lib/lean");
        let image = root.join("libQualifiedPlugin.dylib");
        let compiled = root.join("QualifiedPlugin.olean");
        held.record.check_file(&image).map_err(ioerr)?;
        held.record.check_file(&compiled).map_err(ioerr)?;
        // The sealed tree and order select this project's output first.
        if paths.first().map(std::path::Path::new) != Some(&root) {
            return Err(denied());
        }
        audit("authorized-plugin-and-olean");
        // SAFETY: our private canonical plugin and the selected .olean are one sealed
        // build under Held. The producer inspected the initializer, runtime and loader
        // closure. This image's test registration lives for this worker and is idempotent.
        unsafe { cap.import_scope_with_plugin(paths, names, options, image, "initialize_QualifiedPlugin".into()) }
    }
    fn project(
        &self,
        _: &'static LeanRuntime,
        _: &Path,
        _: &LeanModuleInitializer,
    ) -> LeanResult<(LeanLibrary<'static>, LeanModuleInitializer)> {
        Err(denied())
    }
    fn term<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: String,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedHostTerm<'static, 'cap>> {
        Err(denied())
    }
    fn kernel<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: String,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedHostKernelRequest<'static, 'cap>> {
        Err(denied())
    }
    fn query<'cap>(
        &self,
        session: WorkerSourceContext<'_, 'cap>,
        request: HostModuleQuery,
        options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, HostModuleQuery>> {
        let roots = self.source_roots(&session, &request.source, &options)?;
        if request.query != ModuleQuery::Diagnostics {
            return Err(denied());
        }
        // SAFETY: source_roots binds this session, complete input, roots and options under lifetime custody.
        // Diagnostics is the sole admitted single projection; no native callbacks or arbitrary source.
        audit("grant-single");
        unsafe {
            session
                .capability
                .module_query_scope(session.environment, session.imports, roots, request, options)
        }
    }
    fn batch<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: HostModuleBatch,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, HostModuleBatch>> {
        Err(denied())
    }
    fn cached<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: HostCachedModuleBatch,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, HostCachedModuleBatch>> {
        Err(denied())
    }
    fn proof<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: ProofAttemptRequest,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, ProofAttemptRequest>> {
        Err(denied())
    }
    fn verify<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: DeclarationVerificationRequest,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, DeclarationVerificationRequest>> {
        Err(denied())
    }
    fn verify_batch<'cap>(
        &self,
        _session: WorkerSourceContext<'_, 'cap>,
        _request: DeclarationVerificationBatchRequest,
        _options: HostSourceOptions,
    ) -> LeanResult<TrustedModuleSource<'static, 'cap, DeclarationVerificationBatchRequest>> {
        Err(denied())
    }
}
mod host_entry {
    include!(env!("HOST_PLUGIN_WORKER_ENTRY"));
    pub(super) fn run() -> std::process::ExitCode {
        main()
    }
}

fn main() -> std::process::ExitCode {
    audit("worker-entry");
    if lean_rs_worker_child::install_worker_authority(HostTrial(OnceLock::new())).is_err() {
        return std::process::ExitCode::FAILURE;
    }
    host_entry::run()
}
const SOURCE: &str = "import QualifiedPlugin\nexample : qualifiedPluginWitness = 73 := rfl\n";
mod observed;
impl HostTrial {
    fn source_roots(
        &self,
        session: &WorkerSourceContext<'_, '_>,
        source: &str,
        options: &HostSourceOptions,
    ) -> LeanResult<Vec<String>> {
        let held = self.deployment()?;
        let d = &held.record;
        if source != SOURCE
            || !options.namespace.is_empty()
            || options.file != "HostTrial.lean"
            || options.heartbeats != 100000
            || options.diagnostic_bytes != 8192
            || options.options_json != "{}"
        {
            return Err(denied());
        }
        if session.capability.library_path() != d.host_image {
            return Err(denied());
        }
        session.imports.check_capability(session.capability)?;
        let image = d.project.join(".lake/build/lib/lean/libQualifiedPlugin.dylib");
        if session.imports.plugin() != Some((image.to_str().ok_or_else(denied)?, "initialize_QualifiedPlugin")) {
            return Err(denied());
        }
        let supplied: Vec<_> = d.import_roots.iter().map(|p| p.display().to_string()).collect();
        if !session.imports.matches_request(&supplied, session.imports.imports()) {
            return Err(denied());
        }
        d.check_order(&supplied, session.imports.imports(), session.imports.options())
            .map_err(ioerr)?;
        Ok(d.expected_roots())
    }
}
